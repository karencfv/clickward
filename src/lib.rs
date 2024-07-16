// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::process::{Command, Stdio};

pub mod config;
use config::*;

/// We put things in a subdirectory of the user path for easy cleanup
pub const DEPLOYMENT_DIR: &str = "deployment";

/// The name of the file where `ClickwardMetadata` lives. This is *always*
/// directly below <path>/deployment.
pub const CLICKWARD_META_FILENAME: &str = "clickward-metadata.json";

pub const DEFAULT_BASE_PORTS: BasePorts = BasePorts {
    keeper: 20000,
    raft: 21000,
    clickhouse_tcp: 22000,
    clickhouse_http: 23000,
    clickhouse_interserver_http: 24000,
};

// A configuration for a given clickward deployment
pub struct DeploymentConfig {
    pub path: Utf8PathBuf,
    pub base_ports: BasePorts,
}

impl DeploymentConfig {
    pub fn new_with_default_ports(path: Utf8PathBuf) -> DeploymentConfig {
        let path = path.join(DEPLOYMENT_DIR);
        DeploymentConfig {
            path,
            base_ports: DEFAULT_BASE_PORTS,
        }
    }
}

// Port allocation used for config generation
pub struct BasePorts {
    pub keeper: u16,
    pub raft: u16,
    pub clickhouse_tcp: u16,
    pub clickhouse_http: u16,
    pub clickhouse_interserver_http: u16,
}
/// Metadata stored for use by clickward
///
/// This prevents the need to parse XML and only includes what we need to
/// implement commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickwardMetadata {
    /// IDs of keepers that are currently part of the cluster
    /// We never reuse IDs.
    pub keeper_ids: BTreeSet<u64>,

    /// The maximum allocated keeper_id so far
    /// We only ever increment when adding a new id.
    pub max_keeper_id: u64,

    /// IDs of clickhouse servers
    /// We never reuse IDs.
    pub server_ids: BTreeSet<u64>,

    /// The maximum allocated clickhouse server id so far
    /// We only ever increment when adding a new id.
    pub max_server_id: u64,
}

impl ClickwardMetadata {
    pub fn new(keeper_ids: BTreeSet<u64>, replica_ids: BTreeSet<u64>) -> ClickwardMetadata {
        let max_keeper_id = *keeper_ids.last().unwrap();
        let max_replica_id = *replica_ids.last().unwrap();
        ClickwardMetadata {
            keeper_ids,
            max_keeper_id,
            server_ids: replica_ids,
            max_server_id: max_replica_id,
        }
    }

    pub fn add_keeper(&mut self) -> u64 {
        self.max_keeper_id += 1;
        self.keeper_ids.insert(self.max_keeper_id);
        self.max_keeper_id
    }

    pub fn remove_keeper(&mut self, id: u64) -> Result<()> {
        let was_removed = self.keeper_ids.remove(&id);
        if !was_removed {
            bail!("No such keeper: {id}");
        }
        Ok(())
    }

    pub fn add_server(&mut self) -> u64 {
        self.max_server_id += 1;
        self.server_ids.insert(self.max_server_id);
        self.max_server_id
    }

    pub fn remove_server(&mut self, id: u64) -> Result<()> {
        let was_removed = self.server_ids.remove(&id);
        if !was_removed {
            bail!("No such replica: {id}");
        }
        Ok(())
    }

    pub fn load(deployment_dir: &Utf8Path) -> Result<ClickwardMetadata> {
        let path = deployment_dir.join(CLICKWARD_META_FILENAME);
        let json =
            std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
        let meta = serde_json::from_str(&json)?;
        Ok(meta)
    }

    pub fn save(&self, deployment_dir: &Utf8Path) -> Result<()> {
        let path = deployment_dir.join(CLICKWARD_META_FILENAME);
        let json = serde_json::to_string(self)?;
        std::fs::write(&path, &json).with_context(|| format!("Failed to write {path}"))?;
        Ok(())
    }
}

/// A deployment of Clickhouse servers and Keeper clusters
///
/// This always generates clusters on localhost and is suitable only for testing
pub struct Deployment {
    config: DeploymentConfig,
}

impl Deployment {
    pub fn new_with_default_port_config(path: Utf8PathBuf) -> Deployment {
        let config = DeploymentConfig::new_with_default_ports(path);
        Deployment { config }
    }

    pub fn show(&self) -> Result<()> {
        let meta = ClickwardMetadata::load(&self.config.path)?;
        println!("{:#?}", meta);
        Ok(())
    }

    /// Add a node to clickhouse keeper config at all replicas and start the new
    /// keeper
    pub fn add_keeper(&self) -> Result<()> {
        let path = &self.config.path;
        let mut meta = ClickwardMetadata::load(path)?;
        let new_id = meta.add_keeper();

        println!("Updating config to include new keeper: {new_id}");

        // The writes from the following two functions aren't transactional
        // Don't worry about it.
        //
        // We update the new node and start it before the other nodes. It must be online
        // for reconfiguration to succeed.
        meta.save(path)?;
        self.generate_keeper_config(new_id, meta.keeper_ids.clone())?;
        self.start_keeper(new_id);

        // Generate new configs for all the other keepers
        // They will automatically reload them.
        let mut other_keepers = meta.keeper_ids.clone();
        other_keepers.remove(&new_id);
        for id in other_keepers {
            self.generate_keeper_config(id, meta.keeper_ids.clone())?;
        }

        // Update clickhouse configs so they know about the new keeper node
        self.generate_clickhouse_config(meta.keeper_ids.clone(), meta.server_ids.clone())?;

        Ok(())
    }

    /// Add a new clickhouse server replica
    pub fn add_server(&self) -> Result<()> {
        let mut meta = ClickwardMetadata::load(&self.config.path)?;
        let new_id = meta.add_server();

        println!("Updating config to include new replica: {new_id}");

        // The writes from the following two functions aren't transactional
        // Don't worry about it.
        meta.save(&self.config.path)?;

        // Update clickhouse configs so they know about the new replica
        self.generate_clickhouse_config(meta.keeper_ids.clone(), meta.server_ids.clone())?;

        // Start the new replica
        self.start_server(new_id);

        Ok(())
    }

    /// Remove a node from clickhouse keeper config at all replicas and stop the
    /// old replica.
    pub fn remove_keeper(&self, id: u64) -> Result<()> {
        println!("Updating config to remove keeper: {id}");
        let mut meta = ClickwardMetadata::load(&self.config.path)?;
        meta.remove_keeper(id)?;

        // The writes from the following functions aren't transactional
        // Don't worry about it.
        meta.save(&self.config.path)?;
        for id in &meta.keeper_ids {
            self.generate_keeper_config(*id, meta.keeper_ids.clone())?;
        }
        self.stop_keeper(id)?;

        // Update clickhouse configs so they know about the removed keeper node
        self.generate_clickhouse_config(meta.keeper_ids.clone(), meta.server_ids.clone())?;

        Ok(())
    }

    /// Remove a node from clickhouse server config at all replicas and stop the
    /// old server.
    pub fn remove_server(&self, id: u64) -> Result<()> {
        println!("Updating config to remove clickhouse server: {id}");
        let mut meta = ClickwardMetadata::load(&self.config.path)?;
        meta.remove_server(id)?;

        // The writes from the following functions aren't transactional
        // Don't worry about it.
        meta.save(&self.config.path)?;

        // Update clickhouse configs so they know about the removed keeper node
        self.generate_clickhouse_config(meta.keeper_ids.clone(), meta.server_ids.clone())?;

        // Stop the clickhouse server
        self.stop_server(id)?;

        Ok(())
    }

    /// Get the keeper config from a running keeper
    pub fn keeper_config(&self, id: u64) -> Result<()> {
        let port = self.config.base_ports.keeper + id as u16;
        let mut child = Command::new("clickhouse")
            .arg("keeper-client")
            .arg("--port")
            .arg(port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to connect to keeper client at port {port}"))?;

        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        stdin
            .write_all(b"get /keeper/config\nexit\n")
            .context("failed to send 'get' to keeper")?;

        let mut output = String::new();
        stdout.read_to_string(&mut output)?;
        println!("{output}");

        Ok(())
    }

    pub fn start_keeper(&self, id: u64) {
        let dir = self.config.path.join(format!("keeper-{id}"));
        println!("Deploying keeper: {dir}");
        let config = dir.join("keeper-config.xml");
        let pidfile = dir.join("keeper.pid");
        Command::new("clickhouse")
            .arg("keeper")
            .arg("-C")
            .arg(config)
            .arg("--pidfile")
            .arg(pidfile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start keeper");
    }

    pub fn start_server(&self, id: u64) {
        let dir = self.config.path.join(format!("clickhouse-{id}"));
        println!("Deploying clickhouse server: {dir}");
        let config = dir.join("clickhouse-config.xml");
        Command::new("clickhouse")
            .arg("server")
            .arg("-C")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start clickhouse server");
    }

    pub fn stop_keeper(&self, id: u64) -> Result<()> {
        let dir = self.config.path.join(format!("keeper-{id}"));
        let pidfile = dir.join("keeper.pid");
        let pid = std::fs::read_to_string(&pidfile)?;
        let pid = pid.trim_end();
        println!("Stopping keeper: {dir} at pid {pid}");
        Command::new("kill")
            .arg("-9")
            .arg(pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to kill keeper");
        std::fs::remove_file(&pidfile)?;
        Ok(())
    }

    pub fn stop_server(&self, id: u64) -> Result<()> {
        let name = format!("clickhouse-{id}");
        println!("Stopping clickhouse server: {name}");
        Command::new("pkill")
            .arg("-f")
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to kill clickhouse server");
        Ok(())
    }

    /// Deploy our clickhouse replicas and keeper cluster
    pub fn deploy(&self) -> Result<()> {
        let dirs: Vec<_> = self.config.path.read_dir_utf8()?.collect();

        // Find all keeper replicas them
        let keeper_dirs = dirs.iter().filter_map(|e| {
            let entry = e.as_ref().unwrap();
            if entry.path().file_name().unwrap().starts_with("keeper") {
                Some(entry.path())
            } else {
                None
            }
        });
        // Start all keepers
        for dir in keeper_dirs {
            println!("Deploying keeper: {dir}");
            let config = dir.join("keeper-config.xml");
            let pidfile = dir.join("keeper.pid");
            Command::new("clickhouse")
                .arg("keeper")
                .arg("-C")
                .arg(config)
                .arg("--pidfile")
                .arg(pidfile)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Failed to start keeper");
        }

        // Find all clickhouse replicas
        let clickhouse_dirs = dirs.iter().filter_map(|e| {
            let entry = e.as_ref().unwrap();
            if entry.path().file_name().unwrap().starts_with("clickhouse") {
                Some(entry.path())
            } else {
                None
            }
        });

        // Start all clickhouse servers
        for dir in clickhouse_dirs {
            println!("Deploying clickhouse server: {dir}");
            let config = dir.join("clickhouse-config.xml");
            let pidfile = dir.join("clickhouse.pid");
            Command::new("clickhouse")
                .arg("server")
                .arg("-C")
                .arg(config)
                .arg("--pidfile")
                .arg(pidfile)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Failed to start clickhouse server");
        }
        Ok(())
    }

    /// Generate configuration for our clusters
    pub fn generate_config(&self, num_keepers: u64, num_replicas: u64) -> Result<()> {
        std::fs::create_dir_all(&self.config.path).unwrap();

        let keeper_ids: BTreeSet<u64> = (1..=num_keepers).collect();
        let replica_ids: BTreeSet<u64> = (1..=num_replicas).collect();

        self.generate_clickhouse_config(keeper_ids.clone(), replica_ids.clone())?;
        for id in &keeper_ids {
            self.generate_keeper_config(*id, keeper_ids.clone())?;
        }

        let meta = ClickwardMetadata::new(keeper_ids, replica_ids);
        meta.save(&self.config.path)?;

        Ok(())
    }
    fn generate_clickhouse_config(
        &self,
        keeper_ids: BTreeSet<u64>,
        replica_ids: BTreeSet<u64>,
    ) -> Result<()> {
        let cluster = "test_cluster".to_string();

        let servers: Vec<_> = replica_ids
            .iter()
            .map(|&id| ServerConfig {
                host: "::1".to_string(),
                port: self.config.base_ports.clickhouse_tcp + id as u16,
            })
            .collect();
        let remote_servers = RemoteServers {
            cluster: cluster.clone(),
            secret: "some-unique-value".to_string(),
            replicas: servers,
        };

        let keepers = KeeperConfigsForReplica {
            nodes: keeper_ids
                .iter()
                .map(|&id| ServerConfig {
                    host: "[::1]".to_string(),
                    port: self.config.base_ports.keeper + id as u16,
                })
                .collect(),
        };

        for id in replica_ids {
            let dir: Utf8PathBuf = [self.config.path.as_str(), &format!("clickhouse-{id}")]
                .iter()
                .collect();
            let logs: Utf8PathBuf = dir.join("logs");
            std::fs::create_dir_all(&logs)?;
            let log = logs.join("clickhouse.log");
            let errorlog = logs.join("clickhouse.err.log");
            let data_path = dir.join("data");
            let config = ReplicaConfig {
                logger: LogConfig {
                    level: LogLevel::Trace,
                    log,
                    errorlog,
                    size: "100M".to_string(),
                    count: 1,
                },
                macros: Macros {
                    shard: 1,
                    replica: id,
                    cluster: cluster.clone(),
                },
                listen_host: "::1".to_string(),
                http_port: self.config.base_ports.clickhouse_http + id as u16,
                tcp_port: self.config.base_ports.clickhouse_tcp + id as u16,
                interserver_http_port: self.config.base_ports.clickhouse_interserver_http
                    + id as u16,
                remote_servers: remote_servers.clone(),
                keepers: keepers.clone(),
                data_path,
            };
            let mut f = File::create(dir.join("clickhouse-config.xml"))?;
            f.write_all(config.to_xml().as_bytes())?;
            f.flush()?;
        }
        Ok(())
    }

    /// Generate a config for `this_keeper` consisting of the replicas in `keeper_ids`
    fn generate_keeper_config(&self, this_keeper: u64, keeper_ids: BTreeSet<u64>) -> Result<()> {
        let raft_servers: Vec<_> = keeper_ids
            .iter()
            .map(|id| RaftServerConfig {
                id: *id,
                hostname: "::1".to_string(),
                port: self.config.base_ports.raft + *id as u16,
            })
            .collect();
        let dir: Utf8PathBuf = [self.config.path.as_str(), &format!("keeper-{this_keeper}")]
            .iter()
            .collect();
        let logs: Utf8PathBuf = dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        let log = logs.join("clickhouse-keeper.log");
        let errorlog = logs.join("clickhouse-keeper.err.log");
        let config = KeeperConfig {
            logger: LogConfig {
                level: LogLevel::Trace,
                log,
                errorlog,
                size: "100M".to_string(),
                count: 1,
            },
            listen_host: "::1".to_string(),
            tcp_port: self.config.base_ports.keeper + this_keeper as u16,
            server_id: this_keeper,
            log_storage_path: dir.join("coordination").join("log"),
            snapshot_storage_path: dir.join("coordination").join("snapshots"),
            coordination_settings: KeeperCoordinationSettings {
                operation_timeout_ms: 10000,
                session_timeout_ms: 30000,
                raft_logs_level: LogLevel::Trace,
            },
            raft_config: RaftServers {
                servers: raft_servers.clone(),
            },
        };
        let mut f = File::create(dir.join("keeper-config.xml"))?;
        f.write_all(config.to_xml().as_bytes())?;
        f.flush()?;

        Ok(())
    }
}