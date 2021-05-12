use rand::Rng;
use sha2::{Digest, Sha256};
use structopt::StructOpt;

/// Search for a hash loop of any length.
#[derive(StructOpt)]
struct Opt {
    /// Maximum search length, positive integer
    #[structopt(default_value = "4294967296")]
    max: u128,
    /// Switch on verbosity
    #[structopt(short)]
    verbose: bool,
}

fn fmt_hash(input: &[u8]) -> String {
    input.iter().map(|byte| format!("{:02X}", byte)).collect()
}

fn main() {
    let opt = Opt::from_args();
    let mut rng = rand::thread_rng();
    loop {
        let seed: [u8; 32] = rng.gen();
        println!("{} random hash seed", fmt_hash(&seed));

        let mut slow = Sha256::digest(&seed);
        let mut fast = Sha256::digest(&seed);
        for _ in 0..opt.max {
            if opt.verbose {
                println!("{}", fmt_hash(&slow));
            }
            for _ in 0..2 {
                fast = Sha256::digest(&fast);
                if slow == fast {
                    println!("{} hash found on cycle", fmt_hash(&slow));
                    panic!("Don't panic, the search has ended!")
                }
            }
            slow = Sha256::digest(&slow);
        }
    }
}
