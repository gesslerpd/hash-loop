use rand::Rng;
use rayon::prelude::*;
use sha1::{
    digest::{typenum::Unsigned, OutputSizeUser},
    Digest, Sha1,
};
use std::time::{Duration, Instant};
use structopt::StructOpt;

const HASH_SIZE: usize = <Sha1 as OutputSizeUser>::OutputSize::USIZE;
type Hash = [u8; HASH_SIZE];

/// Search for a hash loop of any length.
#[derive(StructOpt)]
struct Opt {
    /// Maximum search length, positive integer
    #[structopt(default_value = "4294967296")]
    max: u128,
    /// Number of leading SHA-1 bits retained in each state
    #[structopt(long, default_value = "32")]
    bits: u8,
    /// Number of independent seeds to test
    #[structopt(long, default_value = "1")]
    trials: u64,
    /// Optional 160-bit state to replay instead of generating random seeds
    #[structopt(long)]
    seed: Option<String>,
    /// Enumerate the entire truncated state space and find the exact global minimum cycle (bits <= 31)
    #[structopt(long)]
    exhaustive: bool,
    /// Stop exhaustive search after this many seconds and report the best cycle found so far
    #[structopt(long)]
    timeout_secs: Option<u64>,
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

fn find_cycle<F>(seed: Hash, max: u128, hash: F) -> Option<(Hash, u128)>
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

    while tortoise != hare && steps < max {
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

fn main() {
    let opt = Opt::from_args();
    if opt.max == 0 {
        eprintln!("max must be greater than zero");
        std::process::exit(2);
    }
    if opt.trials == 0 {
        eprintln!("trials must be greater than zero");
        std::process::exit(2);
    }
    if opt.seed.is_some() && opt.trials != 1 {
        eprintln!("seed replay requires exactly one trial");
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

    let result = if let Some(seed_text) = opt.seed.as_deref() {
        let seed = match parse_hash(seed_text) {
            Ok(seed) => seed,
            Err(error) => {
                eprintln!("invalid seed: {}", error);
                std::process::exit(2);
            }
        };
        find_cycle(seed, opt.max, |input| sha1_hash(input, opt.bits))
    } else {
        (0..opt.trials)
            .into_par_iter()
            .map(|_| {
                let mut rng = rand::thread_rng();
                let seed = truncate_hash(rng.gen(), opt.bits);
                if opt.verbose {
                    println!("{} random hash seed", fmt_hash(&seed));
                }

                find_cycle(seed, opt.max, |input| sha1_hash(input, opt.bits))
            })
            .filter_map(|cycle| cycle)
            .min_by_key(|(_, cycle_length)| *cycle_length)
    };

    if let Some((cycle_hash, cycle_length)) = result {
        println!(
            "{} {}-bit SHA-1 hash found on cycle of length {} after {} trial(s)",
            fmt_hash(&cycle_hash),
            opt.bits,
            cycle_length,
            opt.trials
        );
    } else {
        eprintln!("no cycle found within the configured search limits");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{find_cycle, min_cycle_in_graph_progressive, Hash, HASH_SIZE};
    use std::time::{Duration, Instant};

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
            find_cycle(seed, 10, toy_hash).expect("cycle should be found");

        assert!(cycle_hash[0] == 2 || cycle_hash[0] == 3);
        assert_eq!(cycle_length, 2);
    }

    #[test]
    fn zero_max_does_not_search() {
        assert!(find_cycle([0; HASH_SIZE], 0, toy_hash).is_none());
    }
}
