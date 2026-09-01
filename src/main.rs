use rand::Rng;
use rayon::prelude::*;
use sha1::{
    digest::{typenum::Unsigned, OutputSizeUser},
    Digest, Sha1,
};
use std::convert::TryFrom;
use std::io::Write;
use std::time::{Duration, Instant};
use structopt::StructOpt;

const HASH_SIZE: usize = <Sha1 as OutputSizeUser>::OutputSize::USIZE;
type Hash = [u8; HASH_SIZE];

/// Search for a hash loop of any length.
#[derive(StructOpt)]
struct Opt {
    /// Maximum search length, positive integer
    #[structopt(default_value = "34359738368")]
    max: u128,
    /// Ignore cycles whose length is not less than this value
    #[structopt(long)]
    max_cycle_length: Option<u128>,
    /// Number of leading SHA-1 bits retained in each state
    #[structopt(long, default_value = "32")]
    bits: u8,
    /// Number of independent seeds to test
    #[structopt(long, default_value = "1")]
    trials: u64,
    /// Number of fresh random seed batches to test on the GPU
    #[structopt(long, default_value = "4")]
    gpu_restarts: u64,
    /// Optional 160-bit state to replay instead of generating random seeds
    #[structopt(long)]
    seed: Option<String>,
    /// Enumerate the entire truncated state space and find the exact global minimum cycle (bits <= 31)
    #[structopt(long)]
    exhaustive: bool,
    /// Stop the search after this many seconds and report the best cycle found so far
    #[structopt(long)]
    timeout_secs: Option<u64>,
    /// Run sampled trials on the selected high-performance GPU
    #[structopt(long)]
    gpu: bool,
    /// Number of sampled trials submitted in one GPU dispatch
    #[structopt(long, default_value = "65536")]
    gpu_batch_size: u64,
    /// Run a fixed-transition GPU throughput benchmark instead of cycle search
    #[structopt(long)]
    gpu_benchmark: bool,
    /// Number of SHA-1 transitions per benchmark trial
    #[structopt(long, default_value = "256")]
    gpu_benchmark_hashes: u32,
    /// CUDA threads per block; benchmark several values for best throughput
    #[structopt(long, default_value = "256")]
    gpu_block_size: u32,
    /// SHA-1 transitions performed per GPU kernel dispatch and trial
    #[structopt(long, default_value = "65536")]
    gpu_steps_per_dispatch: u32,
    /// Switch on verbosity
    #[structopt(short)]
    verbose: bool,
}

fn fmt_hash(input: &[u8]) -> String {
    input.iter().map(|byte| format!("{:02X}", byte)).collect()
}

fn parse_hash(value: &str) -> Result<Hash, String> {
    if value.len() != HASH_SIZE * 2 {
        return Err(format!(
            "seed must contain {} hexadecimal characters",
            HASH_SIZE * 2
        ));
    }

    let mut hash = [0; HASH_SIZE];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0])
            .ok_or_else(|| format!("invalid hexadecimal character at position {}", index * 2))?;
        let low = hex_digit(pair[1]).ok_or_else(|| {
            format!(
                "invalid hexadecimal character at position {}",
                index * 2 + 1
            )
        })?;
        hash[index] = (high << 4) | low;
    }
    Ok(hash)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn truncate_hash(mut hash: Hash, bits: u8) -> Hash {
    let full_bytes = usize::from(bits / 8);
    let partial_bits = bits % 8;
    let retained_bytes = full_bytes + usize::from(partial_bits != 0);

    if partial_bits != 0 {
        hash[full_bytes] &= u8::MAX << (8 - partial_bits);
    }
    for byte in hash.iter_mut().skip(retained_bytes) {
        *byte = 0;
    }
    hash
}

fn sha1_hash(input: &Hash, bits: u8) -> Hash {
    truncate_hash(Sha1::digest(input).into(), bits)
}

// The truncated state space has exactly 2^bits distinct values (bits <= 32), each
// identifiable by a compact u32 index left-aligned into the padded 160-bit state.
fn embed_index(idx: u32, bits: u8) -> Hash {
    let mut hash = [0u8; HASH_SIZE];
    let shifted = if bits < 32 { idx << (32 - bits) } else { idx };
    hash[0..4].copy_from_slice(&shifted.to_be_bytes());
    hash
}

fn extract_index(hash: &Hash, bits: u8) -> u32 {
    let mut word_bytes = [0u8; 4];
    word_bytes.copy_from_slice(&hash[0..4]);
    let word = u32::from_be_bytes(word_bytes);
    if bits < 32 {
        word >> (32 - bits)
    } else {
        word
    }
}

fn next_index(idx: u32, bits: u8) -> u32 {
    extract_index(&sha1_hash(&embed_index(idx, bits), bits), bits)
}

// Finds the exact global minimum cycle in a functional graph given as a `next[]`
// adjacency array, using linear-time path coloring (no randomness, no hash map overhead).
// Reports every improvement to `on_new_best` as soon as it is found, and checks
// `deadline` periodically so a long search can be stopped and still report its best
// result so far. Returns (best, states_processed, completed).
//
// `state[i]` packs both the color and in-path position into one u32 (halving memory vs.
// separate color/position arrays): UNVISITED, DONE, or the node's index within the path
// currently being traced. This requires n < DONE, true for bits <= 31.
fn min_cycle_in_graph_progressive<F: FnMut(u32, u64)>(
    next: &[u32],
    deadline: Option<Instant>,
    mut on_new_best: F,
) -> (Option<(u32, u64)>, u64, bool) {
    const UNVISITED: u32 = u32::MAX;
    const DONE: u32 = u32::MAX - 1;
    let n = next.len();
    let mut state = vec![UNVISITED; n];
    let mut best: Option<(u32, u64)> = None;
    let mut path: Vec<u32> = Vec::new();
    let mut last_progress_report = Instant::now();
    const CHECK_MASK: u32 = (1 << 18) - 1;

    for start in 0..n as u32 {
        if start & CHECK_MASK == 0 {
            let now = Instant::now();
            if let Some(deadline) = deadline {
                if now >= deadline {
                    return (best, start as u64, false);
                }
            }
            if now.duration_since(last_progress_report).as_secs_f64() >= 2.0 {
                eprintln!(
                    "progress: {}/{} states processed ({:.1}%)",
                    start,
                    n,
                    100.0 * start as f64 / n as f64
                );
                last_progress_report = now;
            }
        }

        if state[start as usize] != UNVISITED {
            continue;
        }

        path.clear();
        let mut cursor = start;
        while state[cursor as usize] == UNVISITED {
            state[cursor as usize] = path.len() as u32;
            path.push(cursor);
            cursor = next[cursor as usize];
        }

        if state[cursor as usize] != DONE {
            let idx = state[cursor as usize] as usize;
            let cycle_length = (path.len() - idx) as u64;
            if best.is_none_or(|(_, best_length)| cycle_length < best_length) {
                best = Some((cursor, cycle_length));
                on_new_best(cursor, cycle_length);
            }
        }

        for &node in &path {
            state[node as usize] = DONE;
        }
    }

    (best, n as u64, true)
}

fn exhaustive_min_cycle<F: FnMut(Hash, u64)>(
    bits: u8,
    timeout: Option<Duration>,
    mut on_new_best: F,
) -> (Option<(Hash, u64)>, u64, u64, bool) {
    let n = 1usize << bits;
    let next: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .map(|idx| next_index(idx, bits))
        .collect();

    let deadline = timeout.map(|duration| Instant::now() + duration);
    let (best, states_processed, completed) =
        min_cycle_in_graph_progressive(&next, deadline, |idx, cycle_length| {
            on_new_best(embed_index(idx, bits), cycle_length);
        });

    let best_hash = best.map(|(idx, cycle_length)| (embed_index(idx, bits), cycle_length));
    (best_hash, states_processed, n as u64, completed)
}

fn find_cycle<F>(seed: Hash, max: u128, deadline: Option<Instant>, hash: F) -> Option<(Hash, u128)>
where
    F: Fn(&Hash) -> Hash,
{
    if max == 0 {
        return None;
    }

    let mut tortoise = seed;
    let mut hare = hash(&seed);
    let mut power = 1u128;
    let mut length = 1u128;
    let mut steps = 1u128;
    const CHECK_MASK: u128 = (1 << 20) - 1;

    while tortoise != hare && steps < max {
        if let Some(deadline) = deadline {
            if steps & CHECK_MASK == 0 && Instant::now() >= deadline {
                return None;
            }
        }
        if power == length {
            tortoise = hare;
            power = power.saturating_mul(2);
            length = 0;
        }
        hare = hash(&hare);
        length += 1;
        steps += 1;
    }

    if tortoise != hare {
        return None;
    }

    let cycle_hash = tortoise;
    let mut cycle_length = 1u128;
    let mut cursor = hash(&cycle_hash);
    while cursor != cycle_hash {
        cursor = hash(&cursor);
        cycle_length += 1;
    }

    Some((cycle_hash, cycle_length))
}

const CUDA_SOURCE: &str = r#"
typedef unsigned int uint;
typedef unsigned long long ulong;

__device__ __forceinline__ uint rotate_left(uint value, uint amount) {
    return (value << amount) | (value >> (32u - amount));
}

__device__ __forceinline__ void copy_state(const uint source[5], uint destination[5]) {
    destination[0] = source[0];
    destination[1] = source[1];
    destination[2] = source[2];
    destination[3] = source[3];
    destination[4] = source[4];
}

__device__ __forceinline__ bool same_state(const uint left[5], const uint right[5]) {
    return left[0] == right[0] && left[1] == right[1] && left[2] == right[2]
        && left[3] == right[3] && left[4] == right[4];
}

__device__ __forceinline__ bool below_limit(uint low, uint high, uint max_low, uint max_high) {
    return high < max_high || (high == max_high && low < max_low);
}

__device__ __forceinline__ void increment_counter(uint &low, uint &high) {
    low++;
    if (low == 0u) {
        high++;
    }
}

__device__ __forceinline__ void double_counter(uint &low, uint &high) {
    uint old_low = low;
    uint old_high = high;
    low = old_low << 1u;
    high = (old_high << 1u) | (old_low >> 31u);
    if ((old_high & 0x80000000u) != 0u) {
        low = 0xffffffffu;
        high = 0xffffffffu;
    }
}

__device__ __forceinline__ void sha1_hash(const uint input[5], uint output[5], uint bits) {
    uint schedule[16];
    schedule[0] = input[0];
    schedule[1] = input[1];
    schedule[2] = input[2];
    schedule[3] = input[3];
    schedule[4] = input[4];
    schedule[5] = 0x80000000u;
    schedule[6] = 0u;
    schedule[7] = 0u;
    schedule[8] = 0u;
    schedule[9] = 0u;
    schedule[10] = 0u;
    schedule[11] = 0u;
    schedule[12] = 0u;
    schedule[13] = 0u;
    schedule[14] = 0u;
    schedule[15] = 160u;

    uint a = 0x67452301u;
    uint b = 0xefcdab89u;
    uint c = 0x98badcfeu;
    uint d = 0x10325476u;
    uint e = 0xc3d2e1f0u;
    #pragma unroll 80
    for (uint round = 0u; round < 80u; round++) {
        uint schedule_word = schedule[round & 15u];
        if (round >= 16u) {
            schedule_word = rotate_left(
                schedule[(round - 3u) & 15u] ^ schedule[(round - 8u) & 15u]
                    ^ schedule[(round - 14u) & 15u] ^ schedule[round & 15u],
                1u
            );
            schedule[round & 15u] = schedule_word;
        }

        uint function_value;
        uint constant;
        if (round < 20u) {
            function_value = (b & c) | ((~b) & d);
            constant = 0x5a827999u;
        } else if (round < 40u) {
            function_value = b ^ c ^ d;
            constant = 0x6ed9eba1u;
        } else if (round < 60u) {
            function_value = (b & c) | (b & d) | (c & d);
            constant = 0x8f1bbcdcu;
        } else {
            function_value = b ^ c ^ d;
            constant = 0xca62c1d6u;
        }
        uint temporary = rotate_left(a, 5u) + function_value + e + constant + schedule_word;
        e = d;
        d = c;
        c = rotate_left(b, 30u);
        b = a;
        a = temporary;
    }

    output[0] = 0x67452301u + a;
    output[1] = 0xefcdab89u + b;
    output[2] = 0x98badcfeu + c;
    output[3] = 0x10325476u + d;
    output[4] = 0xc3d2e1f0u + e;

    uint full_words = bits / 32u;
    uint partial_bits = bits & 31u;
    if (partial_bits != 0u) {
        output[full_words] &= 0xffffffffu << (32u - partial_bits);
        for (uint index = full_words + 1u; index < 5u; index++) {
            output[index] = 0u;
        }
    } else {
        for (uint index = full_words; index < 5u; index++) {
            output[index] = 0u;
        }
    }
}

extern "C" __global__ void find_cycles_chunk(
    const uint *seeds,
    uint *states,
    uint *results,
    uint bits,
    uint max_low,
    uint max_high,
    uint cycle_limit_low,
    uint cycle_limit_high,
    uint trial_count,
    uint steps_per_dispatch
) {
    uint trial = blockIdx.x * blockDim.x + threadIdx.x;
    if (trial >= trial_count) {
        return;
    }

    uint *global_state = states + trial * 17u;
    uint *result = results + trial * 8u;
    const uint *seed = seeds + trial * 5u;

    // Cache the per-trial state in registers for the whole dispatch instead of
    // re-issuing a global load/store every loop iteration.
    uint state[17];
    #pragma unroll
    for (uint index = 0u; index < 17u; index++) {
        state[index] = global_state[index];
    }

    if (state[16] == 4u) {
        state[0] = seed[0];
        state[1] = seed[1];
        state[2] = seed[2];
        state[3] = seed[3];
        state[4] = seed[4];
        sha1_hash(state, state + 5u, bits);
        state[10] = 1u;
        state[11] = 0u;
        state[12] = 1u;
        state[13] = 0u;
        state[14] = 1u;
        state[15] = 0u;
        state[16] = 0u;
        result[7] = 0u;
    }

    if (state[16] < 2u) {
        uint next[5];
        for (uint iteration = 0u; iteration < steps_per_dispatch; iteration++) {
            if (state[16] == 0u) {
                if (same_state(state, state + 5u)) {
                    sha1_hash(state, state + 5u, bits);
                    state[10] = 0u;
                    state[11] = 0u;
                    state[12] = 1u;
                    state[13] = 0u;
                    state[16] = 1u;
                    continue;
                }
                if (!below_limit(state[14], state[15], max_low, max_high)) {
                    state[16] = 3u;
                    break;
                }
                if (state[10] == state[12] && state[11] == state[13]) {
                    copy_state(state + 5u, state);
                    double_counter(state[10], state[11]);
                    state[12] = 0u;
                    state[13] = 0u;
                }
                sha1_hash(state + 5u, next, bits);
                copy_state(next, state + 5u);
                increment_counter(state[12], state[13]);
                increment_counter(state[14], state[15]);
            } else {
                if (same_state(state + 5u, state)) {
                    if (!below_limit(state[12], state[13], cycle_limit_low, cycle_limit_high)) {
                        state[16] = 3u;
                        break;
                    }
                    result[0] = state[0];
                    result[1] = state[1];
                    result[2] = state[2];
                    result[3] = state[3];
                    result[4] = state[4];
                    result[5] = state[12];
                    result[6] = state[13];
                    result[7] = 1u;
                    state[16] = 2u;
                    break;
                }
                if (!below_limit(state[12], state[13], cycle_limit_low, cycle_limit_high)) {
                    state[16] = 3u;
                    break;
                }
                sha1_hash(state + 5u, next, bits);
                copy_state(next, state + 5u);
                increment_counter(state[12], state[13]);
            }
        }

        if (state[16] == 3u) {
            result[7] = 2u;
        }
    }

    #pragma unroll
    for (uint index = 0u; index < 17u; index++) {
        global_state[index] = state[index];
    }
}

extern "C" __global__ void benchmark_hashes(
    const uint *seeds,
    uint *outputs,
    uint bits,
    uint iterations,
    uint trial_count
) {
    uint trial = blockIdx.x * blockDim.x + threadIdx.x;
    if (trial >= trial_count) {
        return;
    }

    uint state[5];
    uint next[5];
    const uint *seed = seeds + trial * 5u;
    state[0] = seed[0];
    state[1] = seed[1];
    state[2] = seed[2];
    state[3] = seed[3];
    state[4] = seed[4];
    for (uint iteration = 0u; iteration < iterations; iteration++) {
        sha1_hash(state, next, bits);
        copy_state(next, state);
    }
    outputs[trial] = state[0] ^ state[1] ^ state[2] ^ state[3] ^ state[4];
}
"#;

fn cuda_device() -> Result<
    (
        std::sync::Arc<cudarc::driver::CudaContext>,
        std::sync::Arc<cudarc::driver::CudaModule>,
    ),
    String,
> {
    use cudarc::driver::CudaContext;
    use cudarc::nvrtc::{compile_ptx_with_opts, CompileOptions};

    let context =
        CudaContext::new(0).map_err(|error| format!("could not open CUDA device 0: {}", error))?;
    let ptx = compile_ptx_with_opts(
        CUDA_SOURCE,
        CompileOptions {
            arch: Some("compute_86"),
            ..Default::default()
        },
    )
    .map_err(|error| format!("could not compile CUDA kernel with NVRTC: {}", error))?;
    let module = context
        .load_module(ptx)
        .map_err(|error| format!("could not load CUDA PTX: {}", error))?;
    Ok((context, module))
}

fn cuda_launch_config(trials: usize, block_size: u32) -> cudarc::driver::LaunchConfig {
    cudarc::driver::LaunchConfig {
        grid_dim: (((trials as u32) + block_size - 1) / block_size, 1, 1),
        block_dim: (block_size, 1, 1),
        shared_mem_bytes: 0,
    }
}

fn flatten_seeds(seeds: &[Hash]) -> Vec<u32> {
    seeds
        .iter()
        .flat_map(|seed| {
            [
                u32::from_be_bytes([seed[0], seed[1], seed[2], seed[3]]),
                u32::from_be_bytes([seed[4], seed[5], seed[6], seed[7]]),
                u32::from_be_bytes([seed[8], seed[9], seed[10], seed[11]]),
                u32::from_be_bytes([seed[12], seed[13], seed[14], seed[15]]),
                u32::from_be_bytes([seed[16], seed[17], seed[18], seed[19]]),
            ]
        })
        .collect()
}

fn cuda_find_cycles(
    on_new_best: impl FnMut(Hash, u128),
    seeds: &[Hash],
    bits: u8,
    max: u128,
    max_cycle_length: Option<u128>,
    batch_size: usize,
    block_size: u32,
    steps_per_dispatch: u32,
    verbose: bool,
    deadline: Option<Instant>,
) -> Result<Option<(Hash, u128)>, String> {
    use cudarc::driver::PushKernelArg;
    let mut on_new_best = on_new_best;

    if max > u128::from(u64::MAX) {
        return Err("CUDA search supports maximum lengths up to 2^64 - 1".to_string());
    }
    if max_cycle_length.is_some_and(|length| length > u128::from(u64::MAX)) {
        return Err("CUDA cycle-length cutoff supports values up to 2^64 - 1".to_string());
    }
    if seeds.len() > u32::MAX as usize || batch_size > u32::MAX as usize {
        return Err("CUDA search supports at most 2^32 - 1 trials per dispatch".to_string());
    }
    let (context, module) = cuda_device()?;
    let function = module
        .load_function("find_cycles_chunk")
        .map_err(|error| format!("could not load CUDA cycle kernel: {}", error))?;
    let stream = context.default_stream();
    if verbose {
        eprintln!(
            "CUDA device 0: {}",
            context.name().unwrap_or_else(|_| "unknown".to_string())
        );
    }
    let max = max as u64;
    let mut cycle_limit = max_cycle_length.unwrap_or(u128::from(u64::MAX)) as u64;
    let mut best: Option<(Hash, u128)> = None;
    for batch in seeds.chunks(batch_size) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let seed_words = flatten_seeds(batch);
        let device_seeds = stream
            .clone_htod(&seed_words)
            .map_err(|error| format!("could not upload CUDA seeds: {}", error))?;
        let mut state_words = vec![0u32; batch.len() * 17];
        for trial in 0..batch.len() {
            let state_offset = trial * 17;
            let seed_offset = trial * 5;
            state_words[state_offset..state_offset + 5]
                .copy_from_slice(&seed_words[seed_offset..seed_offset + 5]);
            state_words[state_offset + 16] = 4;
        }
        let mut device_states = stream
            .clone_htod(&state_words)
            .map_err(|error| format!("could not upload CUDA cycle state: {}", error))?;
        let mut device_results = stream
            .alloc_zeros::<u32>(batch.len() * 8)
            .map_err(|error| format!("could not allocate CUDA results: {}", error))?;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            let config = cuda_launch_config(batch.len(), block_size);
            unsafe {
                stream
                    .launch_builder(&function)
                    .arg(&device_seeds)
                    .arg(&mut device_states)
                    .arg(&mut device_results)
                    .arg(&u32::from(bits))
                    .arg(&(max as u32))
                    .arg(&((max >> 32) as u32))
                    .arg(&(cycle_limit as u32))
                    .arg(&((cycle_limit >> 32) as u32))
                    .arg(&(batch.len() as u32))
                    .arg(&steps_per_dispatch)
                    .launch(config)
                    .map_err(|error| format!("CUDA cycle kernel failed: {}", error))?;
            }
            stream
                .synchronize()
                .map_err(|error| format!("CUDA synchronization failed: {}", error))?;
            let results = stream
                .clone_dtoh(&device_results)
                .map_err(|error| format!("could not download CUDA results: {}", error))?;
            let mut completed = 0;
            for result in results.chunks_exact(8) {
                if result[7] == 0 {
                    continue;
                }
                completed += 1;
                if result[7] != 1 {
                    continue;
                }
                let mut cycle_hash = [0u8; HASH_SIZE];
                for index in 0..5 {
                    cycle_hash[index * 4..index * 4 + 4]
                        .copy_from_slice(&result[index].to_be_bytes());
                }
                let cycle_length = u128::from(result[5]) | (u128::from(result[6]) << 32);
                if best.is_none_or(|(_, best_length)| cycle_length < best_length) {
                    on_new_best(cycle_hash, cycle_length);
                    best = Some((cycle_hash, cycle_length));
                    cycle_limit = cycle_length as u64;
                }
            }
            if completed == batch.len() {
                break;
            }
        }
    }
    Ok(best)
}

fn cuda_benchmark(
    bits: u8,
    trials: usize,
    batch_size: usize,
    block_size: u32,
    iterations: u32,
    verbose: bool,
) -> Result<(u128, f64, u32), String> {
    use cudarc::driver::PushKernelArg;

    if trials > u32::MAX as usize || batch_size > u32::MAX as usize {
        return Err("CUDA benchmark supports at most 2^32 - 1 trials per dispatch".to_string());
    }
    if iterations == 0 {
        return Err("GPU benchmark iterations must be greater than zero".to_string());
    }
    let (context, module) = cuda_device()?;
    let benchmark_function = module
        .load_function("benchmark_hashes")
        .map_err(|error| format!("could not load CUDA benchmark kernel: {}", error))?;
    let stream = context.default_stream();
    if verbose {
        eprintln!(
            "CUDA device 0: {}",
            context.name().unwrap_or_else(|_| "unknown".to_string())
        );
    }
    let mut rng = rand::thread_rng();
    let seed_words: Vec<u32> = (0..trials * 5).map(|_| rng.gen()).collect();
    let warmup_len = trials.min(batch_size);
    let warmup_seeds = stream
        .clone_htod(&seed_words[..warmup_len * 5])
        .map_err(|error| format!("could not upload CUDA benchmark seeds: {}", error))?;
    let mut warmup_results = stream
        .alloc_zeros::<u32>(warmup_len)
        .map_err(|error| format!("could not allocate CUDA benchmark results: {}", error))?;
    unsafe {
        stream
            .launch_builder(&benchmark_function)
            .arg(&warmup_seeds)
            .arg(&mut warmup_results)
            .arg(&u32::from(bits))
            .arg(&iterations)
            .arg(&(warmup_len as u32))
            .launch(cuda_launch_config(warmup_len, block_size))
            .map_err(|error| format!("CUDA benchmark warmup failed: {}", error))?;
    }
    stream
        .synchronize()
        .map_err(|error| format!("CUDA warmup synchronization failed: {}", error))?;

    let mut sink = 0u32;
    let mut elapsed = 0.0;
    for start in (0..trials).step_by(batch_size) {
        let count = (trials - start).min(batch_size);
        let device_seeds = stream
            .clone_htod(&seed_words[start * 5..(start + count) * 5])
            .map_err(|error| format!("could not upload CUDA benchmark seeds: {}", error))?;
        let mut device_results = stream
            .alloc_zeros::<u32>(count)
            .map_err(|error| format!("could not allocate CUDA benchmark results: {}", error))?;
        let begin = Instant::now();
        unsafe {
            stream
                .launch_builder(&benchmark_function)
                .arg(&device_seeds)
                .arg(&mut device_results)
                .arg(&u32::from(bits))
                .arg(&iterations)
                .arg(&(count as u32))
                .launch(cuda_launch_config(count, block_size))
                .map_err(|error| format!("CUDA benchmark kernel failed: {}", error))?;
        }
        stream
            .synchronize()
            .map_err(|error| format!("CUDA benchmark synchronization failed: {}", error))?;
        elapsed += begin.elapsed().as_secs_f64();
        let results = stream
            .clone_dtoh(&device_results)
            .map_err(|error| format!("could not download CUDA benchmark results: {}", error))?;
        for value in results {
            sink ^= value;
        }
    }
    let total_hashes = (trials as u128) * u128::from(iterations);
    Ok((total_hashes, elapsed, sink))
}

fn main() {
    let opt = Opt::from_args();
    if opt.max == 0 {
        eprintln!("max must be greater than zero");
        std::process::exit(2);
    }
    if opt.max_cycle_length == Some(0) {
        eprintln!("max-cycle-length must be greater than zero");
        std::process::exit(2);
    }
    if opt.trials == 0 {
        eprintln!("trials must be greater than zero");
        std::process::exit(2);
    }
    if opt.gpu_restarts == 0 {
        eprintln!("gpu-restarts must be greater than zero");
        std::process::exit(2);
    }
    if opt.seed.is_some() && opt.trials != 1 {
        eprintln!("seed replay requires exactly one trial");
        std::process::exit(2);
    }
    if opt.gpu && opt.seed.is_some() && opt.gpu_restarts != 1 {
        eprintln!("GPU seed replay requires exactly one GPU restart");
        std::process::exit(2);
    }
    if !(1..=160).contains(&opt.bits) {
        eprintln!("bits must be between 1 and 160");
        std::process::exit(2);
    }
    if opt.exhaustive && opt.seed.is_some() {
        eprintln!("exhaustive search does not accept a seed");
        std::process::exit(2);
    }
    if opt.exhaustive && opt.bits > 31 {
        eprintln!("exhaustive search only supports bits <= 31 (2^31 states)");
        std::process::exit(2);
    }
    if opt.gpu_batch_size == 0 {
        eprintln!("gpu-batch-size must be greater than zero");
        std::process::exit(2);
    }
    if opt.gpu_steps_per_dispatch == 0 {
        eprintln!("gpu-steps-per-dispatch must be greater than zero");
        std::process::exit(2);
    }
    if opt.gpu && opt.exhaustive {
        eprintln!("GPU acceleration is available for sampled trials, not exhaustive search");
        std::process::exit(2);
    }
    if (opt.gpu || opt.gpu_benchmark)
        && (opt.gpu_block_size < 32
            || opt.gpu_block_size > 1024
            || !opt.gpu_block_size.is_power_of_two())
    {
        eprintln!("gpu-block-size must be a power of two between 32 and 1024");
        std::process::exit(2);
    }
    if opt.gpu_benchmark_hashes == 0 {
        eprintln!("gpu-benchmark-hashes must be greater than zero");
        std::process::exit(2);
    }
    if opt.gpu_benchmark && opt.exhaustive {
        eprintln!("GPU benchmark does not support exhaustive search");
        std::process::exit(2);
    }
    if opt.gpu_benchmark && opt.seed.is_some() {
        eprintln!("GPU benchmark does not accept a seed");
        std::process::exit(2);
    }

    if opt.exhaustive {
        let timeout = opt.timeout_secs.map(Duration::from_secs);
        let (best, states_processed, total_states, completed) =
            exhaustive_min_cycle(opt.bits, timeout, |cycle_hash, cycle_length| {
                println!(
                    "{} {}-bit SHA-1 hash found on cycle of length {} (new best so far)",
                    fmt_hash(&cycle_hash),
                    opt.bits,
                    cycle_length
                );
            });

        match best {
            Some((cycle_hash, cycle_length)) if completed => println!(
                "{} {}-bit SHA-1 hash found on cycle of length {} (exhaustive, global minimum)",
                fmt_hash(&cycle_hash),
                opt.bits,
                cycle_length
            ),
            Some((cycle_hash, cycle_length)) => println!(
                "{} {}-bit SHA-1 hash found on cycle of length {} (exhaustive, partial: {}/{} states processed, not proven global minimum)",
                fmt_hash(&cycle_hash),
                opt.bits,
                cycle_length,
                states_processed,
                total_states
            ),
            None => {
                eprintln!(
                    "exhaustive search stopped after {} of {} states without finding a cycle",
                    states_processed, total_states
                );
                std::process::exit(1);
            }
        }
        return;
    }

    if opt.gpu_benchmark {
        let trials = match usize::try_from(opt.trials) {
            Ok(trials) if trials > 0 => trials,
            _ => {
                eprintln!("trials is too large for this platform");
                std::process::exit(2);
            }
        };
        let batch_size = match usize::try_from(opt.gpu_batch_size) {
            Ok(batch_size) if batch_size > 0 => batch_size,
            _ => {
                eprintln!("gpu-batch-size is too large for this platform");
                std::process::exit(2);
            }
        };
        match cuda_benchmark(
            opt.bits,
            trials,
            batch_size,
            opt.gpu_block_size,
            opt.gpu_benchmark_hashes,
            opt.verbose,
        ) {
            Ok((total_hashes, elapsed, sink)) => println!(
                "GPU benchmark: {} trials x {} SHA-1 transitions = {} hashes in {:.3}s = {:.2}M hashes/sec (sink: {:08X})",
                opt.trials,
                opt.gpu_benchmark_hashes,
                total_hashes,
                elapsed,
                total_hashes as f64 / elapsed / 1e6,
                sink
            ),
            Err(error) => {
                eprintln!("GPU benchmark failed: {}", error);
                std::process::exit(1);
            }
        }
        return;
    }

    let sample_deadline = opt
        .timeout_secs
        .map(|secs| Instant::now() + Duration::from_secs(secs));

    let result = if opt.gpu {
        let total_gpu_trials = match opt.trials.checked_mul(opt.gpu_restarts) {
            Some(total_trials) => total_trials,
            None => {
                eprintln!("trials multiplied by gpu-restarts is too large");
                std::process::exit(2);
            }
        };
        let seeds = if let Some(seed_text) = opt.seed.as_deref() {
            let seed = match parse_hash(seed_text) {
                Ok(seed) => seed,
                Err(error) => {
                    eprintln!("invalid seed: {}", error);
                    std::process::exit(2);
                }
            };
            vec![seed]
        } else {
            (0..total_gpu_trials)
                .map(|_| {
                    let mut rng = rand::thread_rng();
                    let seed = truncate_hash(rng.gen(), opt.bits);
                    if opt.verbose {
                        println!("{} random hash seed", fmt_hash(&seed));
                    }
                    seed
                })
                .collect()
        };
        let gpu_batch_size = match usize::try_from(opt.gpu_batch_size) {
            Ok(batch_size) if batch_size > 0 => batch_size,
            _ => {
                eprintln!("gpu-batch-size is too large for this platform");
                std::process::exit(2);
            }
        };
        match cuda_find_cycles(
            |cycle_hash, cycle_length| {
                println!(
                    "{} {}-bit SHA-1 hash found on cycle of length {} (GPU new best so far)",
                    fmt_hash(&cycle_hash),
                    opt.bits,
                    cycle_length
                );
                let _ = std::io::stdout().flush();
            },
            &seeds,
            opt.bits,
            opt.max,
            opt.max_cycle_length,
            gpu_batch_size,
            opt.gpu_block_size,
            opt.gpu_steps_per_dispatch,
            opt.verbose,
            sample_deadline,
        ) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("GPU search failed: {}", error);
                std::process::exit(1);
            }
        }
    } else if let Some(seed_text) = opt.seed.as_deref() {
        let seed = match parse_hash(seed_text) {
            Ok(seed) => seed,
            Err(error) => {
                eprintln!("invalid seed: {}", error);
                std::process::exit(2);
            }
        };
        find_cycle(seed, opt.max, sample_deadline, |input| {
            sha1_hash(input, opt.bits)
        })
        .filter(|(_, cycle_length)| {
            opt.max_cycle_length
                .is_none_or(|limit| *cycle_length < limit)
        })
    } else {
        (0..opt.trials)
            .into_par_iter()
            .map(|_| {
                let mut rng = rand::thread_rng();
                let seed = truncate_hash(rng.gen(), opt.bits);
                if opt.verbose {
                    println!("{} random hash seed", fmt_hash(&seed));
                }

                find_cycle(seed, opt.max, sample_deadline, |input| {
                    sha1_hash(input, opt.bits)
                })
                .filter(|(_, cycle_length)| {
                    opt.max_cycle_length
                        .is_none_or(|limit| *cycle_length < limit)
                })
            })
            .filter_map(|cycle| cycle)
            .min_by_key(|(_, cycle_length)| *cycle_length)
    };

    let reported_trials = if opt.gpu {
        opt.trials.saturating_mul(opt.gpu_restarts)
    } else {
        opt.trials
    };

    if let Some((cycle_hash, cycle_length)) = result {
        println!(
            "{} {}-bit SHA-1 hash found on cycle of length {} after {} trial(s)",
            fmt_hash(&cycle_hash),
            opt.bits,
            cycle_length,
            reported_trials
        );
    } else {
        eprintln!("no cycle found within the configured search limits");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        find_cycle, min_cycle_in_graph_progressive, next_index, sha1_hash, Hash, Opt, HASH_SIZE,
    };
    use rayon::prelude::*;
    use std::time::{Duration, Instant};
    use structopt::StructOpt;

    #[test]
    fn default_search_limit_reaches_ten_billion_cycle_lengths() {
        let opt = Opt::from_iter_safe(["hash-loop"]).expect("default arguments should parse");
        assert!(opt.max >= 10_300_411_851);
        assert_eq!(opt.gpu_restarts, 4);
        assert_eq!(opt.gpu_steps_per_dispatch, 65_536);
    }

    #[test]
    fn min_cycle_in_graph_finds_global_minimum() {
        let next: Vec<u32> = (0..8u32).map(|x| (x * 3 + 1) % 8).collect();
        let (best, states_processed, completed) =
            min_cycle_in_graph_progressive(&next, None, |_, _| {});
        let (_, cycle_length) = best.expect("cycle should be found");
        assert_eq!(cycle_length, 4);
        assert_eq!(states_processed, 8);
        assert!(completed);
    }

    #[test]
    fn min_cycle_in_graph_respects_deadline() {
        let next: Vec<u32> = (0..8u32).map(|x| (x * 3 + 1) % 8).collect();
        let past_deadline = Instant::now() - Duration::from_secs(1);
        let (best, states_processed, completed) =
            min_cycle_in_graph_progressive(&next, Some(past_deadline), |_, _| {});
        assert!(!completed);
        assert_eq!(states_processed, 0);
        assert!(best.is_none());
    }

    fn toy_hash(input: &Hash) -> Hash {
        let mut output = *input;
        output[0] = match input[0] {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => 2,
        };
        output
    }

    #[test]
    fn finds_a_cycle() {
        let seed = [0; HASH_SIZE];
        let (cycle_hash, cycle_length) =
            find_cycle(seed, 10, None, toy_hash).expect("cycle should be found");

        assert!(cycle_hash[0] == 2 || cycle_hash[0] == 3);
        assert_eq!(cycle_length, 2);
    }

    #[test]
    fn zero_max_does_not_search() {
        assert!(find_cycle([0; HASH_SIZE], 0, None, toy_hash).is_none());
    }

    // Throughput benchmark, not part of normal test runs: `cargo test --release -- --ignored
    // --nocapture bench_hash_throughput`. Used to verify the impact of compiler/hardware
    // acceleration changes on real hashes/sec rather than assuming.
    #[test]
    #[ignore]
    fn bench_hash_throughput() {
        let seed = [0u8; HASH_SIZE];

        let single_thread_iters = 20_000_000u64;
        let start = Instant::now();
        let mut state = seed;
        for _ in 0..single_thread_iters {
            state = sha1_hash(&state, 32);
        }
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "single-thread: {} hashes in {:.3}s = {:.2}M hashes/sec (sink: {:02X})",
            single_thread_iters,
            elapsed,
            single_thread_iters as f64 / elapsed / 1e6,
            state[0]
        );

        let multi_thread_iters = 200_000_000u64;
        let start = Instant::now();
        let sink: u8 = (0..multi_thread_iters)
            .into_par_iter()
            .map(|i| next_index(i as u32, 32) as u8)
            .reduce(|| 0, |a, b| a ^ b);
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "multi-thread ({} cores): {} hashes in {:.3}s = {:.2}M hashes/sec (sink: {:02X})",
            rayon::current_num_threads(),
            multi_thread_iters,
            elapsed,
            multi_thread_iters as f64 / elapsed / 1e6,
            sink
        );
    }
}
