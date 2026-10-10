//! chainsim: local mainnet forks in Docker, with a JSON interface for agents.

fn main() {
    let profiles = chainsim_core::profile::all().map(|ps| ps.into_iter().map(|p| p.key).collect::<Vec<_>>());
    let (out, code) = chainsim_core::envelope::render(&profiles);
    println!("{out}");
    std::process::exit(code);
}
