//! Docker control through the Docker API (bollard). A fork is one engine
//! container plus one control sidecar on one network. The Docker labels on
//! the engine container are the fork registry: chainsim keeps no state file
//! (design section 4).

use std::collections::HashMap;
use std::time::Duration;

use bollard::models::{ContainerCreateBody, EndpointSettings, HostConfig, NetworkCreateRequest, NetworkingConfig, PortBinding};
use bollard::query_parameters::{
    BuildImageOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder, ListNetworksOptionsBuilder,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;

use crate::error::{Error, ErrorCode, Result};
use crate::fork::{Fork, ForkPoint};
use crate::lock::{host_arch, Engine, EngineLock, CONTROL_ENGINE};
use crate::profile::Profile;

pub const LABEL_FORK: &str = "org.chainsim.fork";
pub const LABEL_CHAIN: &str = "org.chainsim.chain";
pub const LABEL_ENGINE: &str = "org.chainsim.engine";
pub const LABEL_FORK_POINT: &str = "org.chainsim.fork-point";
pub const LABEL_ROLE: &str = "org.chainsim.role";
pub const LABEL_NETWORK: &str = "org.chainsim.network";
pub const LABEL_ALIAS: &str = "org.chainsim.alias";
pub const LABEL_NETWORK_CREATED: &str = "org.chainsim.network-created";
/// On a volume: the `uid:gid` it was chowned to when created.
pub const LABEL_OWNER: &str = "org.chainsim.owner";

pub const ROLE_ENGINE: &str = "engine";
pub const ROLE_CONTROL: &str = "control";
/// A short-lived container that prepares a fork volume.
pub const ROLE_INIT: &str = "init";

/// The engine's memory limit.
pub const ENGINE_MEMORY: i64 = 1 << 30;
const CONTROL_MEMORY: i64 = 64 << 20;

/// A fork name: lowercase letters, digits and `-`, up to 40 characters.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::new(ErrorCode::Usage, format!("bad fork name {name:?}: use lowercase letters, digits and -, up to 40 characters")))
    }
}

pub fn engine_container(name: &str) -> String {
    format!("chainsim-{name}")
}

pub fn control_container(name: &str) -> String {
    format!("chainsim-{name}-control")
}

pub fn default_network(name: &str) -> String {
    format!("chainsim-{name}")
}

/// A running (or stopped) fork, read back from its container labels.
#[derive(Debug, Clone)]
pub struct ForkHandle {
    pub name: String,
    pub chain: String,
    pub engine: String,
    pub fork_point: ForkPoint,
    pub network: String,
    pub alias: String,
    pub network_created: bool,
    pub engine_container: String,
    pub control_container: String,
    pub running: bool,
    /// Seconds since the engine started.
    pub uptime_secs: u64,
    /// The `127.0.0.1` port, when published.
    pub host_port: Option<u16>,
}

impl ForkHandle {
    /// The in-network RPC URL of the engine.
    pub fn rpc_url(&self, profile: &Profile) -> String {
        format!("http://{}:{}", self.alias, profile.ports.rpc)
    }

    /// The fork object, rebuilt from the labels and the profile.
    pub fn fork(&self, profile: &Profile, dev_accounts: Option<Vec<String>>) -> Fork {
        Fork {
            name: self.name.clone(),
            chain: profile.key.clone(),
            chain_id: profile.id.clone(),
            maturity: profile.maturity,
            fork_point: self.fork_point.clone(),
            network: self.network.clone(),
            alias: self.alias.clone(),
            rpc_url: self.rpc_url(profile),
            ws_url: profile.ports.ws.map(|p| format!("ws://{}:{p}", self.alias)),
            host_rpc_url: self.host_port.map(|p| format!("http://127.0.0.1:{p}")),
            dev_accounts,
            upstream: profile.upstream_info(),
            limits: profile.limits.clone(),
        }
    }
}

/// What a container runs. Every container chainsim starts is hardened:
/// read-only root, tmpfs scratch, all capabilities dropped,
/// no-new-privileges, a memory limit (design section 8).
#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub user: Option<String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Vec<String>,
    /// `KEY=value`. May hold the upstream URL; never logged.
    pub env: Vec<String>,
    pub labels: HashMap<String, String>,
    pub network: String,
    pub aliases: Vec<String>,
    /// Paths mounted as tmpfs, with mount options.
    pub tmpfs: Vec<(String, String)>,
    pub memory: i64,
    /// Publish this container port on a free `127.0.0.1` port.
    pub publish: Option<u16>,
    /// Named volumes and their mount paths. Each is created if missing and
    /// labeled with the fork (the `org.chainsim.fork` label of the spec), so
    /// it outlives a container restart and `remove_fork` removes it.
    pub mounts: Vec<(String, String)>,
}

/// The output of one `docker exec`.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

pub struct Docker {
    inner: bollard::Docker,
}

fn docker_err(e: bollard::errors::Error) -> Error {
    // bollard's text stops at "client error (Connect)": the cause, such as
    // "Permission denied (os error 13)", is further down the source chain.
    let mut message = format!("docker: {e}");
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    let next = docker_next(&message);
    Error::new(ErrorCode::DockerUnavailable, message).next(next)
}

/// `error.next` for docker_unavailable. A refused socket usually means a
/// sandbox, such as Codex's default one, sits between chainsim and Docker; a
/// missing daemon needs Docker started.
pub fn docker_next(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    if ["permission denied", "operation not permitted", "os error 13", "os error 1)"].iter().any(|w| lower.contains(w)) {
        "if this runs inside a sandbox such as Codex's, rerun the command outside the sandbox (approve the escalation); otherwise start Docker and give your user access to its socket, then retry"
    } else {
        "start Docker, then retry"
    }
}

fn is_not_found(e: &bollard::errors::Error) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code: 404, .. })
}

impl Docker {
    /// Connects through `DOCKER_HOST`, else the local socket, and pings.
    pub async fn connect() -> Result<Self> {
        let inner = if std::env::var_os("DOCKER_HOST").is_some() {
            bollard::Docker::connect_with_defaults()
        } else {
            let home_sock = std::env::var("HOME").map(|h| format!("{h}/.docker/run/docker.sock")).unwrap_or_default();
            if std::path::Path::new("/var/run/docker.sock").exists() || home_sock.is_empty() || !std::path::Path::new(&home_sock).exists() {
                bollard::Docker::connect_with_local_defaults()
            } else {
                bollard::Docker::connect_with_socket(&home_sock, 120, bollard::API_DEFAULT_VERSION)
            }
        }
        .map_err(docker_err)?;
        inner.ping().await.map_err(docker_err)?;
        Ok(Self { inner })
    }

    pub fn api(&self) -> &bollard::Docker {
        &self.inner
    }

    /// True when the image is on this host.
    pub async fn image_exists(&self, reference: &str) -> Result<bool> {
        match self.inner.inspect_image(reference).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(docker_err(e)),
        }
    }

    /// Pulls or builds an engine on first use and returns the image reference.
    pub async fn ensure_engine(&self, id: &str, engine: &Engine) -> Result<String> {
        self.pull_engine(id, engine).await.map(|(reference, _)| reference)
    }

    /// Pulls or builds an engine unless it is here, tags it
    /// `chainsim/<id>:<version>` so prune can find it, and returns the image
    /// reference with `present`, `pulled` or `built`.
    pub async fn pull_engine(&self, id: &str, engine: &Engine) -> Result<(String, &'static str)> {
        let reference = engine.reference();
        let status = if self.image_exists(&reference).await? {
            "present"
        } else {
            self.fetch_engine(id, engine, &reference).await?;
            if engine.pull.is_some() { "pulled" } else { "built" }
        };
        let tag = crate::engines::owned_tag(id, &engine.version);
        if tag != reference {
            let (repo, version) = tag.rsplit_once(':').expect("owned tags have a version");
            let opts = bollard::query_parameters::TagImageOptionsBuilder::default().repo(repo).tag(version).build();
            self.inner.tag_image(&reference, Some(opts)).await.map_err(docker_err)?;
        }
        Ok((reference, status))
    }

    async fn fetch_engine(&self, id: &str, engine: &Engine, reference: &str) -> Result<()> {
        let reference = reference.to_string();
        if engine.pull.is_some() {
            crate::log(format_args!("pulling {id} {}", engine.version));
            let opts = CreateImageOptionsBuilder::default().from_image(&reference).build();
            let mut stream = self.inner.create_image(Some(opts), None, None);
            while let Some(item) = stream.next().await {
                item.map_err(|e| Error::new(ErrorCode::EngineFailed, format!("pull {id}: {e}")).next(format!("docker pull {reference}")))?;
            }
        } else if let Some(build) = &engine.build {
            let arch = host_arch();
            let tarball = build.tarballs.get(arch).ok_or_else(|| Error::new(ErrorCode::Unsupported, format!("{id} has no tarball for {arch}")).next("use another chain"))?;
            let dockerfile = crate::embedded::DOCKERFILES
                .iter()
                .find(|(ctx, _)| *ctx == build.context)
                .map(|(_, body)| *body)
                .ok_or_else(|| Error::new(ErrorCode::Internal, format!("engines/{}/Dockerfile is not embedded", build.context)))?;
            crate::log(format_args!("building {id} {} for {arch}", engine.version));
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(dockerfile.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "Dockerfile", dockerfile.as_bytes()).map_err(|e| Error::new(ErrorCode::Internal, e.to_string()))?;
            let context = tar.into_inner().map_err(|e| Error::new(ErrorCode::Internal, e.to_string()))?;
            let args = HashMap::from([
                ("BASE".to_string(), build.base.clone()),
                ("TARBALL_URL".to_string(), tarball.url.clone()),
                ("TARBALL_SHA256_URL".to_string(), tarball.sha256_url.clone()),
                ("TARBALL_SHA256".to_string(), tarball.sha256.clone()),
            ]);
            let labels = HashMap::from([(LABEL_ENGINE.to_string(), format!("{id}@{}", engine.version))]);
            let opts = BuildImageOptionsBuilder::default().dockerfile("Dockerfile").t(&build.image).rm(true).buildargs(&args).labels(&labels).build();
            let mut stream = self.inner.build_image(opts, None, Some(bollard::body_full(context.into())));
            let mut log = String::new();
            while let Some(item) = stream.next().await {
                let info = item.map_err(|e| Error::new(ErrorCode::EngineFailed, format!("build {id}: {e}")))?;
                if let Some(s) = info.stream {
                    log.push_str(&s);
                }
                if let Some(detail) = info.error_detail {
                    let tail: String = log.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                    return Err(Error::new(ErrorCode::EngineFailed, format!("build {id}: {} {tail}", detail.message.unwrap_or_default())));
                }
            }
        }
        if !self.image_exists(&reference).await? {
            return Err(Error::new(ErrorCode::EngineFailed, format!("{id} image {reference} is missing after pull or build")));
        }
        Ok(())
    }

    /// The local `chainsim/*` image tags.
    pub async fn owned_tags(&self) -> Result<Vec<String>> {
        let filters = HashMap::from([("reference".to_string(), vec![format!("{}*", crate::engines::OWNED_REPO)])]);
        let opts = bollard::query_parameters::ListImagesOptionsBuilder::default().filters(&filters).build();
        let images = self.inner.list_images(Some(opts)).await.map_err(docker_err)?;
        Ok(images.into_iter().flat_map(|i| i.repo_tags).filter(|t| t.starts_with(crate::engines::OWNED_REPO)).collect())
    }

    /// Volumes labeled with a fork, with that fork's name.
    pub async fn fork_volumes(&self) -> Result<Vec<(String, String)>> {
        let filters = HashMap::from([("label".to_string(), vec![LABEL_FORK.to_string()])]);
        let vols = self.inner.list_volumes(Some(bollard::query_parameters::ListVolumesOptionsBuilder::default().filters(&filters).build())).await.map_err(docker_err)?;
        Ok(vols.volumes.unwrap_or_default().into_iter().filter_map(|v| v.labels.get(LABEL_FORK).cloned().map(|f| (v.name, f))).collect())
    }

    /// Forks that have a container of any role, running or not.
    pub async fn live_forks(&self) -> Result<std::collections::HashSet<String>> {
        let filters = HashMap::from([("label".to_string(), vec![LABEL_FORK.to_string()])]);
        let opts = ListContainersOptionsBuilder::default().all(true).filters(&filters).build();
        let list = self.inner.list_containers(Some(opts)).await.map_err(docker_err)?;
        Ok(list.into_iter().filter_map(|c| c.labels.and_then(|l| l.get(LABEL_FORK).cloned())).collect())
    }

    /// Removes an image tag or reference, never by force: an image in use stays.
    pub async fn remove_image(&self, image: &str) -> std::result::Result<(), String> {
        self.inner.remove_image(image, None::<bollard::query_parameters::RemoveImageOptions>, None).await.map(|_| ()).map_err(|e| e.to_string())
    }

    /// Removes a volume. A volume in use stays.
    pub async fn remove_volume(&self, volume: &str) -> std::result::Result<(), String> {
        self.inner.remove_volume(volume, None::<bollard::query_parameters::RemoveVolumeOptions>).await.map_err(|e| e.to_string())
    }

    /// Creates the network unless it exists. Returns true when chainsim created it.
    pub async fn ensure_network(&self, network: &str, fork: &str) -> Result<bool> {
        match self.inner.inspect_network(network, None::<bollard::query_parameters::InspectNetworkOptions>).await {
            Ok(_) => return Ok(false),
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(docker_err(e)),
        }
        let req = NetworkCreateRequest {
            name: network.to_string(),
            driver: Some("bridge".into()),
            labels: Some(HashMap::from([(LABEL_FORK.to_string(), fork.to_string())])),
            ..Default::default()
        };
        self.inner.create_network(req).await.map_err(docker_err)?;
        Ok(true)
    }

    /// True when the network exists.
    pub async fn network_exists(&self, network: &str) -> Result<bool> {
        match self.inner.inspect_network(network, None::<bollard::query_parameters::InspectNetworkOptions>).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(docker_err(e)),
        }
    }

    /// Creates a named volume labeled with the fork, unless it exists.
    pub async fn ensure_volume(&self, volume: &str, fork: &str) -> Result<()> {
        self.ensure_volume_labels(volume, fork).await.map(|_| ())
    }

    /// Creates the volume, labeled with the fork, unless it exists, and returns its labels.
    async fn ensure_volume_labels(&self, volume: &str, fork: &str) -> Result<HashMap<String, String>> {
        match self.inner.inspect_volume(volume).await {
            Ok(v) => return Ok(v.labels),
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(docker_err(e)),
        }
        let labels = HashMap::from([(LABEL_FORK.to_string(), fork.to_string())]);
        self.create_volume(volume, labels.clone()).await?;
        Ok(labels)
    }

    async fn create_volume(&self, volume: &str, labels: HashMap<String, String>) -> Result<()> {
        let req = bollard::models::VolumeCreateRequest { name: Some(volume.to_string()), labels: Some(labels), ..Default::default() };
        self.inner.create_volume(req).await.map_err(docker_err)?;
        Ok(())
    }

    /// Creates a named volume labeled with the fork and owned by `owner`
    /// (`uid:gid`), so a non-root engine can write it. The owner is applied
    /// once, when the volume is created: an existing volume is left as is.
    /// Call it before `run`, whose mount then finds the volume in place and
    /// mounts it with `nocopy`, so Docker does not copy the image directory at
    /// the mount path, and its owner, over the volume.
    ///
    /// The chown runs in a short-lived container from the lock-pinned control
    /// image, as root with only CAP_CHOWN and no network.
    pub async fn ensure_volume_owned(&self, lock: &EngineLock, volume: &str, fork: &str, owner: &str) -> Result<()> {
        let valid = owner.split_once(':').is_some_and(|(u, g)| [u, g].iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())));
        if !valid {
            return Err(Error::new(ErrorCode::Internal, format!("volume owner {owner:?} is not uid:gid")));
        }
        if self.volume_exists(volume).await? {
            return Ok(());
        }
        self.create_volume(volume, HashMap::from([(LABEL_FORK.to_string(), fork.to_string()), (LABEL_OWNER.to_string(), owner.to_string())])).await?;
        let chowned = self.chown_volume(lock, volume, fork, owner).await;
        if chowned.is_err() {
            // Without its owner the volume is unusable; the next up retries from scratch.
            let _ = self.inner.remove_volume(volume, None::<bollard::query_parameters::RemoveVolumeOptions>).await;
        }
        chowned
    }

    async fn chown_volume(&self, lock: &EngineLock, volume: &str, fork: &str, owner: &str) -> Result<()> {
        const MOUNT: &str = "/volume";
        let image = self.ensure_engine(CONTROL_ENGINE, lock.engine(CONTROL_ENGINE)?).await?;
        let name = format!("{volume}-chown");
        self.remove_container(&name).await?;
        let body = ContainerCreateBody {
            image: Some(image),
            user: Some("0:0".into()),
            entrypoint: Some(vec!["chown".into()]),
            cmd: Some(vec![owner.to_string(), MOUNT.to_string()]),
            labels: Some(HashMap::from([(LABEL_FORK.to_string(), fork.to_string()), (LABEL_ROLE.to_string(), ROLE_INIT.to_string())])),
            host_config: Some(HostConfig {
                readonly_rootfs: Some(true),
                cap_drop: Some(vec!["ALL".into()]),
                cap_add: Some(vec!["CHOWN".into()]),
                security_opt: Some(vec!["no-new-privileges".into()]),
                memory: Some(CONTROL_MEMORY),
                network_mode: Some("none".into()),
                binds: Some(vec![format!("{volume}:{MOUNT}")]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::default().name(&name).build();
        let created = self.inner.create_container(Some(opts), body).await.map_err(|e| Error::new(ErrorCode::EngineFailed, format!("create {name}: {e}")))?;
        let result = async {
            self.inner
                .start_container(&created.id, None::<bollard::query_parameters::StartContainerOptions>)
                .await
                .map_err(|e| Error::new(ErrorCode::EngineFailed, format!("start {name}: {e}")))?;
            let mut wait = self.inner.wait_container(&created.id, None::<bollard::query_parameters::WaitContainerOptions>);
            let code = match wait.next().await {
                Some(Ok(r)) => r.status_code,
                Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => code,
                Some(Err(e)) => return Err(docker_err(e)),
                None => -1,
            };
            if code != 0 {
                let log = self.logs_tail(&created.id, 5).await;
                return Err(Error::new(ErrorCode::EngineFailed, format!("chown {owner} on volume {volume} exited {code}: {}", log.trim())));
            }
            Ok(())
        }
        .await;
        self.remove_container(&created.id).await?;
        result
    }

    /// True when the volume exists.
    pub async fn volume_exists(&self, volume: &str) -> Result<bool> {
        match self.inner.inspect_volume(volume).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(docker_err(e)),
        }
    }

    /// Creates and starts a hardened container.
    pub async fn run(&self, spec: &ContainerSpec) -> Result<String> {
        let mut binds = Vec::new();
        if !spec.mounts.is_empty() {
            let fork = spec.labels.get(LABEL_FORK).ok_or_else(|| Error::new(ErrorCode::Internal, format!("{} mounts volumes but has no {LABEL_FORK} label", spec.name)))?;
            for (volume, path) in &spec.mounts {
                let labels = self.ensure_volume_labels(volume, fork).await?;
                let nocopy = if labels.contains_key(LABEL_OWNER) { ":nocopy" } else { "" };
                binds.push(format!("{volume}:{path}{nocopy}"));
            }
        }
        let mut port_bindings = None;
        let mut exposed = None;
        if let Some(port) = spec.publish {
            let key = format!("{port}/tcp");
            port_bindings = Some(HashMap::from([(key.clone(), Some(vec![PortBinding { host_ip: Some("127.0.0.1".into()), host_port: Some(String::new()) }]))]));
            exposed = Some(vec![key]);
        }
        let host_config = HostConfig {
            readonly_rootfs: Some(true),
            tmpfs: Some(spec.tmpfs.iter().cloned().collect()),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges".into()]),
            memory: Some(spec.memory),
            network_mode: Some(spec.network.clone()),
            port_bindings,
            binds: (!binds.is_empty()).then_some(binds),
            ..Default::default()
        };
        let endpoint = EndpointSettings { aliases: (!spec.aliases.is_empty()).then(|| spec.aliases.clone()), ..Default::default() };
        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            user: spec.user.clone(),
            entrypoint: spec.entrypoint.clone(),
            cmd: Some(spec.cmd.clone()),
            env: Some(spec.env.clone()),
            labels: Some(spec.labels.clone()),
            exposed_ports: exposed,
            host_config: Some(host_config),
            networking_config: Some(NetworkingConfig { endpoints_config: Some(HashMap::from([(spec.network.clone(), endpoint)])) }),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::default().name(&spec.name).build();
        let created = self.inner.create_container(Some(opts), body).await.map_err(|e| Error::new(ErrorCode::EngineFailed, format!("create {}: {e}", spec.name)))?;
        self.inner
            .start_container(&created.id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .map_err(|e| Error::new(ErrorCode::EngineFailed, format!("start {}: {e}", spec.name)))?;
        Ok(created.id)
    }

    /// Runs a command in a container and collects its output.
    pub async fn exec(&self, container: &str, argv: &[String]) -> Result<ExecOutput> {
        let opts = CreateExecOptions::<String> { attach_stdout: Some(true), attach_stderr: Some(true), cmd: Some(argv.to_vec()), ..Default::default() };
        let exec = self.inner.create_exec(container, opts).await.map_err(|e| {
            if is_not_found(&e) {
                Error::new(ErrorCode::EngineFailed, format!("container {container} is gone")).next("chainsim down <name> then chainsim up <chain>")
            } else {
                docker_err(e)
            }
        })?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let StartExecResults::Attached { mut output, .. } = self.inner.start_exec(&exec.id, None).await.map_err(docker_err)? {
            while let Some(chunk) = output.next().await {
                match chunk.map_err(docker_err)? {
                    bollard::container::LogOutput::StdOut { message } => stdout.push_str(&String::from_utf8_lossy(&message)),
                    bollard::container::LogOutput::StdErr { message } => stderr.push_str(&String::from_utf8_lossy(&message)),
                    _ => {}
                }
            }
        }
        let inspect = self.inner.inspect_exec(&exec.id).await.map_err(docker_err)?;
        Ok(ExecOutput { exit_code: inspect.exit_code.unwrap_or(-1), stdout, stderr })
    }

    /// The last lines of a container's log, for error messages. Callers redact it.
    pub async fn logs_tail(&self, container: &str, lines: usize) -> String {
        let opts = LogsOptionsBuilder::default().stdout(true).stderr(true).tail(&lines.to_string()).build();
        let mut stream = self.inner.logs(container, Some(opts));
        let mut out = String::new();
        while let Some(Ok(chunk)) = stream.next().await {
            out.push_str(&chunk.to_string());
        }
        out
    }

    /// True while the container runs.
    pub async fn is_running(&self, container: &str) -> bool {
        match self.inner.inspect_container(container, None::<bollard::query_parameters::InspectContainerOptions>).await {
            Ok(c) => c.state.and_then(|s| s.running).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Every fork, from the engine containers' labels.
    pub async fn forks(&self) -> Result<Vec<ForkHandle>> {
        let filters = HashMap::from([("label".to_string(), vec![format!("{LABEL_ROLE}={ROLE_ENGINE}")])]);
        let opts = ListContainersOptionsBuilder::default().all(true).filters(&filters).build();
        let list = self.inner.list_containers(Some(opts)).await.map_err(docker_err)?;
        let mut out = Vec::new();
        for c in list {
            let Some(id) = c.id else { continue };
            if let Some(h) = self.handle(&id).await? {
                out.push(h);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// One fork by name.
    pub async fn find(&self, name: &str) -> Result<Option<ForkHandle>> {
        self.handle(&engine_container(name)).await
    }

    async fn handle(&self, container: &str) -> Result<Option<ForkHandle>> {
        let c = match self.inner.inspect_container(container, None::<bollard::query_parameters::InspectContainerOptions>).await {
            Ok(c) => c,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(docker_err(e)),
        };
        let labels = c.config.as_ref().and_then(|cfg| cfg.labels.clone()).unwrap_or_default();
        if labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_ENGINE) {
            return Ok(None);
        }
        let get = |k: &str| labels.get(k).cloned().unwrap_or_default();
        let name = get(LABEL_FORK);
        let (kind, number) = get(LABEL_FORK_POINT).split_once(':').map(|(k, n)| (k.to_string(), n.parse().unwrap_or(0))).unwrap_or_default();
        let state = c.state.clone().unwrap_or_default();
        let running = state.running.unwrap_or(false);
        let uptime_secs = state
            .started_at
            .as_deref()
            .and_then(parse_rfc3339_secs)
            .map(|start| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|now| now.as_secs().saturating_sub(start)).unwrap_or(0))
            .unwrap_or(0);
        let host_port = c
            .network_settings
            .as_ref()
            .and_then(|n| n.ports.as_ref())
            .and_then(|ports| ports.values().flatten().flatten().find(|b| b.host_ip.as_deref() == Some("127.0.0.1")).and_then(|b| b.host_port.as_ref()?.parse().ok()));
        Ok(Some(ForkHandle {
            control_container: control_container(&name),
            engine_container: engine_container(&name),
            name,
            chain: get(LABEL_CHAIN),
            engine: get(LABEL_ENGINE),
            fork_point: ForkPoint { kind, number },
            network: get(LABEL_NETWORK),
            alias: get(LABEL_ALIAS),
            network_created: get(LABEL_NETWORK_CREATED) == "true",
            running,
            uptime_secs,
            host_port,
        }))
    }

    async fn remove_container(&self, container: &str) -> Result<bool> {
        let opts = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
        match self.inner.remove_container(container, Some(opts)).await {
            Ok(()) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(docker_err(e)),
        }
    }

    /// Removes the engine, the sidecar, the fork's volumes, and its network if
    /// chainsim created it. Returns false when nothing was there.
    pub async fn remove_fork(&self, name: &str) -> Result<bool> {
        let handle = self.find(name).await?;
        let engine = self.remove_container(&engine_container(name)).await?;
        let control = self.remove_container(&control_container(name)).await?;
        // Volumes labeled with the fork (Solana and Sui revert state).
        let filters = HashMap::from([("label".to_string(), vec![format!("{LABEL_FORK}={name}")])]);
        let vols = self
            .inner
            .list_volumes(Some(bollard::query_parameters::ListVolumesOptionsBuilder::default().filters(&filters).build()))
            .await
            .map_err(docker_err)?;
        for v in vols.volumes.unwrap_or_default() {
            let _ = self.inner.remove_volume(&v.name, None::<bollard::query_parameters::RemoveVolumeOptions>).await;
        }
        // The network: the one chainsim created for this fork, also when the
        // engine container was already gone.
        let network = match &handle {
            Some(h) if h.network_created => Some(h.network.clone()),
            Some(_) => None,
            None => Some(default_network(name)),
        };
        let mut net_removed = false;
        if let Some(net) = network {
            let filters = HashMap::from([("label".to_string(), vec![format!("{LABEL_FORK}={name}")]), ("name".to_string(), vec![net.clone()])]);
            let nets = self.inner.list_networks(Some(ListNetworksOptionsBuilder::default().filters(&filters).build())).await.map_err(docker_err)?;
            for n in nets {
                if n.name.as_deref() == Some(net.as_str()) {
                    match self.inner.remove_network(&net).await {
                        Ok(()) => net_removed = true,
                        Err(e) if is_not_found(&e) => {}
                        Err(e) => return Err(docker_err(e)),
                    }
                }
            }
        }
        Ok(engine || control || net_removed)
    }

    /// Starts the control sidecar on the fork network. It holds the upstream
    /// URL in its environment for the upstream checks, and nothing else.
    pub async fn run_control(&self, lock: &EngineLock, engine_id: &str, fork: &str, network: &str, upstream: &str) -> Result<String> {
        let engine = lock.engine(engine_id)?;
        let image = self.ensure_engine(engine_id, engine).await?;
        let spec = ContainerSpec {
            name: control_container(fork),
            image,
            user: engine.user.clone(),
            entrypoint: Some(vec!["sleep".into()]),
            cmd: vec!["infinity".into()],
            env: vec![format!("{UPSTREAM_ENV}={upstream}")],
            labels: HashMap::from([(LABEL_FORK.to_string(), fork.to_string()), (LABEL_ROLE.to_string(), ROLE_CONTROL.to_string())]),
            network: network.to_string(),
            aliases: vec![],
            tmpfs: vec![("/tmp".into(), "rw,noexec,nosuid,size=16m".into())],
            memory: CONTROL_MEMORY,
            publish: None,
            mounts: vec![],
        };
        self.run(&spec).await
    }

    /// Waits until `probe` returns true, up to `timeout`.
    pub async fn wait_for<F, Fut>(&self, timeout: Duration, mut probe: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if probe().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        false
    }
}

/// The variable in the engine and sidecar environment that holds the upstream URL.
pub const UPSTREAM_ENV: &str = "CHAINSIM_UPSTREAM";

/// Seconds since the epoch for an RFC 3339 time such as
/// `2026-10-09T21:00:00.123456789Z`.
fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.trim_end_matches('Z');
    let time = time.split(['+']).next()?;
    let mut t = time.split(':');
    let (hh, mm) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let ss = t.next()?.split('.').next()?.parse::<i64>().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    (secs > 0).then_some(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_socket_points_out_of_the_sandbox() {
        let sandbox = docker_next("docker: Error in the hyper legacy client: client error (Connect): Permission denied (os error 13)");
        assert!(sandbox.starts_with("if this runs inside a sandbox such as Codex's, rerun the command outside the sandbox"), "{sandbox}");
        assert!(docker_next("docker: Operation not permitted (os error 1)").contains("sandbox"));
        assert_eq!(docker_next("docker: Socket not found: /var/run/docker.sock"), "start Docker, then retry");
        assert_eq!(docker_next("docker: Connection refused (os error 111)"), "start Docker, then retry");
    }

    #[test]
    fn names() {
        assert!(check_name("arc-mainnet").is_ok());
        assert!(check_name("Arc").is_err());
        assert!(check_name("").is_err());
        assert!(check_name("-x").is_err());
        assert_eq!(engine_container("a"), "chainsim-a");
        assert_eq!(control_container("a"), "chainsim-a-control");
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_secs("1970-01-02T00:00:00Z"), Some(86400));
        assert_eq!(parse_rfc3339_secs("2026-10-10T03:16:29.5Z"), Some(1791602189));
    }
}
