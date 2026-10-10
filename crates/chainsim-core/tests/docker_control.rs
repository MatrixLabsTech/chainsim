//! Needs Docker and network access: `cargo test -p chainsim-core -- --ignored`.
//! Starts a control sidecar on its own network, calls the upstream through it,
//! then removes everything.

use std::time::Duration;

use chainsim_core::control::{Control, Target};
use chainsim_core::docker::{self, Docker};
use chainsim_core::lock::EngineLock;

#[tokio::test]
#[ignore]
async fn sidecar_reaches_the_upstream_and_down_cleans_up() {
    let docker = Docker::connect().await.unwrap();
    let lock = EngineLock::embedded().unwrap();
    let name = "core-smoke";
    docker.remove_fork(name).await.unwrap();
    let network = docker::default_network(name);
    assert!(docker.ensure_network(&network, name).await.unwrap());
    docker.run_control(&lock, name, &network, "https://rpc.mainnet.arc.io").await.unwrap();

    let control = Control::new(&docker, docker::control_container(name), Some("https://rpc.mainnet.arc.io".into()));
    let id = control.rpc(&Target::Upstream, "eth_chainId", serde_json::json!([]), Duration::from_secs(20)).await.unwrap();
    assert_eq!(id, "0x13b2");

    // Nothing listens there: an engine failure, not a hang.
    let err = control.rpc(&Target::Engine("http://nobody:8545".into()), "eth_chainId", serde_json::json!([]), Duration::from_secs(5)).await.unwrap_err();
    assert!(!err.message.contains("http://"), "{}", err.message);

    // The network was created without an engine container, so remove it by hand here.
    docker.remove_fork(name).await.unwrap();
    assert!(!docker.network_exists(&network).await.unwrap());
}
