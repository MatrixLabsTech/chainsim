//! The REST endpoint of an Aptos fork, run as the engine container's main
//! process. See `chainsim_aptos::rest`.

fn main() {
    if let Err(e) = chainsim_aptos::rest::serve() {
        eprintln!("chainsim-aptos-rest: {e}");
        std::process::exit(1);
    }
}
