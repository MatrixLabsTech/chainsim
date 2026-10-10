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
    docker.run_control(&lock, chainsim_core::lock::CONTROL_ENGINE, name, &network, "https://rpc.mainnet.arc.io").await.unwrap();

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

#[tokio::test]
#[ignore]
async fn labeled_volume_mounts_and_remove_fork_deletes_it() {
    use std::collections::HashMap;

    let docker = Docker::connect().await.unwrap();
    let lock = EngineLock::embedded().unwrap();
    let name = "core-vol";
    let volume = "chainsim-core-vol-data";
    docker.remove_fork(name).await.unwrap();
    let network = docker::default_network(name);
    docker.ensure_network(&network, name).await.unwrap();
    let image = docker.ensure_engine(chainsim_core::lock::CONTROL_ENGINE, lock.engine(chainsim_core::lock::CONTROL_ENGINE).unwrap()).await.unwrap();
    let spec = docker::ContainerSpec {
        name: docker::engine_container(name),
        image,
        // Root, so it can write the fresh volume; the hardening flags still apply.
        user: Some("0".into()),
        entrypoint: Some(vec!["sh".into(), "-c".into()]),
        cmd: vec!["echo kept > /data/state && sleep infinity".into()],
        labels: HashMap::from([
            (docker::LABEL_FORK.to_string(), name.to_string()),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_ENGINE.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.clone()),
            (docker::LABEL_NETWORK_CREATED.to_string(), "true".to_string()),
        ]),
        network: network.clone(),
        memory: 64 << 20,
        mounts: vec![(volume.to_string(), "/data".to_string())],
        ..Default::default()
    };
    docker.run(&spec).await.unwrap();
    assert!(docker.volume_exists(volume).await.unwrap());
    let labels = docker.api().inspect_volume(volume).await.unwrap().labels;
    assert_eq!(labels.get(docker::LABEL_FORK).map(String::as_str), Some(name));

    // The volume outlives the container: a second container reads what the first wrote.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    docker.api().remove_container(&spec.name, Some(bollard::query_parameters::RemoveContainerOptionsBuilder::default().force(true).build())).await.unwrap();
    let reader = docker::ContainerSpec { cmd: vec!["cat /data/state && sleep infinity".into()], ..spec.clone() };
    docker.run(&reader).await.unwrap();
    let out = docker.exec(&reader.name, &["cat".into(), "/data/state".into()]).await.unwrap();
    assert_eq!(out.stdout.trim(), "kept");

    assert!(docker.remove_fork(name).await.unwrap());
    assert!(!docker.volume_exists(volume).await.unwrap());
    assert!(!docker.network_exists(&network).await.unwrap());
}

/// A hardened engine spec running as `user` with `volume` at `path`.
fn writer_spec(image: &str, fork: &str, network: &str, user: &str, volume: &str, path: &str) -> chainsim_core::docker::ContainerSpec {
    use std::collections::HashMap;
    docker::ContainerSpec {
        name: docker::engine_container(fork),
        image: image.to_string(),
        user: Some(user.into()),
        entrypoint: Some(vec!["sh".into(), "-c".into()]),
        cmd: vec!["sleep infinity".into()],
        labels: HashMap::from([
            (docker::LABEL_FORK.to_string(), fork.to_string()),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_ENGINE.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.to_string()),
            (docker::LABEL_NETWORK_CREATED.to_string(), "true".to_string()),
        ]),
        network: network.to_string(),
        memory: 64 << 20,
        mounts: vec![(volume.to_string(), path.to_string())],
        ..Default::default()
    }
}

#[tokio::test]
#[ignore]
async fn owned_volume_is_writable_by_a_non_root_engine() {
    let docker = Docker::connect().await.unwrap();
    let lock = EngineLock::embedded().unwrap();
    let image = docker.ensure_engine(chainsim_core::lock::CONTROL_ENGINE, lock.engine(chainsim_core::lock::CONTROL_ENGINE).unwrap()).await.unwrap();
    let write = |name: &str| ["sh".to_string(), "-c".to_string(), format!("echo ok > {name}/state && cat {name}/state")];

    // Without an owner the fresh volume is root's, so uid 65534 cannot write it.
    let name = "core-noown";
    docker.remove_fork(name).await.unwrap();
    let network = docker::default_network(name);
    docker.ensure_network(&network, name).await.unwrap();
    docker.run(&writer_spec(&image, name, &network, "65534:65534", "chainsim-core-noown-data", "/data")).await.unwrap();
    let out = docker.exec(&docker::engine_container(name), &write("/data")).await.unwrap();
    assert_ne!(out.exit_code, 0, "a root-owned volume took a write from 65534");
    assert!(docker.remove_fork(name).await.unwrap());

    // With an owner, at a path absent from the image and at one present in it (/mnt).
    for (name, path) in [("core-own", "/data"), ("core-own-mnt", "/mnt")] {
        let volume = format!("chainsim-{name}-data");
        docker.remove_fork(name).await.unwrap();
        let network = docker::default_network(name);
        docker.ensure_network(&network, name).await.unwrap();
        docker.ensure_volume_owned(&lock, &volume, name, "65534:65534").await.unwrap();
        // A second call leaves the existing volume alone.
        docker.ensure_volume_owned(&lock, &volume, name, "65534:65534").await.unwrap();
        let labels = docker.api().inspect_volume(&volume).await.unwrap().labels;
        assert_eq!(labels.get(docker::LABEL_FORK).map(String::as_str), Some(name));
        assert!(!docker.is_running(&format!("{volume}-chown")).await);

        docker.run(&writer_spec(&image, name, &network, "65534:65534", &volume, path)).await.unwrap();
        let out = docker.exec(&docker::engine_container(name), &write(path)).await.unwrap();
        assert_eq!((out.exit_code, out.stdout.trim()), (0, "ok"), "{path}: {}", out.stderr);

        assert!(docker.remove_fork(name).await.unwrap());
        assert!(!docker.volume_exists(&volume).await.unwrap());
        assert!(!docker.network_exists(&network).await.unwrap());
    }

    assert!(docker.ensure_volume_owned(&lock, "chainsim-core-bad-data", "core-bad", "nobody").await.is_err());
    assert!(!docker.volume_exists("chainsim-core-bad-data").await.unwrap());
}
