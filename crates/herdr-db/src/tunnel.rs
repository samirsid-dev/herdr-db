//! Pre-connection commands (`kubectl port-forward`...), shared by every pane
//! of a source through leases, in `$HERDR_PLUGIN_STATE_DIR/tunnels/<source>/`.
//!
//! 1. A pane takes the directory lock.
//! 2. If the recorded tunnel PID is alive and the local port answers, it adds
//!    its lease (`leases/<holder>`, holding its own PID) and releases the lock.
//! 3. Otherwise it starts the command in its own process group, logs its
//!    output, waits for the port to accept a TCP connection (15 s at most),
//!    then records the PID and its lease.
//! 4. On close the pane removes its lease; the last one stops the tunnel's
//!    process group (SIGTERM).
//! 5. A lease whose PID no longer exists is dead: a crashed pane never keeps
//!    a tunnel alive forever.
//!
//! A local port already taken by something else than our tunnel is an error:
//! better than silently connecting to the wrong database.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const START_TIMEOUT: Duration = Duration::from_secs(15);

pub trait Probe {
    fn alive(&self, pid: u32) -> bool;
    fn port_open(&self, host: &str, port: u16) -> bool;
}

pub struct SystemProbe;

impl Probe for SystemProbe {
    fn alive(&self, pid: u32) -> bool {
        pid_alive(pid)
    }

    fn port_open(&self, host: &str, port: u16) -> bool {
        let Ok(addresses) = (host, port).to_socket_addrs() else {
            return false;
        };
        addresses.into_iter().any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
    }
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn terminate_group(pid: u32) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    // SAFETY: the tunnel runs in its own process group whose id is its PID.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TunnelRecord {
    pid: u32,
    host: String,
    port: u16,
    command: Vec<String>,
    started_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Reuse,
    /// Our tunnel process lives but its port does not answer: replace it.
    Restart,
    PortTaken,
    Start,
}

fn decide(tunnel_alive: bool, port_open: bool) -> Decision {
    match (tunnel_alive, port_open) {
        (true, true) => Decision::Reuse,
        (true, false) => Decision::Restart,
        (false, true) => Decision::PortTaken,
        (false, false) => Decision::Start,
    }
}

pub struct Spec<'a> {
    pub source: &'a str,
    pub command: &'a [String],
    pub host: &'a str,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct Tunnels {
    root: PathBuf,
}

/// Exclusive lock on a source's tunnel directory.
struct DirLock(#[allow(dead_code)] File);

impl Tunnels {
    pub fn new(state_dir: &Path) -> Tunnels {
        Tunnels { root: state_dir.join("tunnels") }
    }

    fn dir(&self, source: &str) -> PathBuf {
        self.root.join(source)
    }

    fn lock(&self, source: &str) -> Result<DirLock> {
        let dir = self.dir(source);
        herdr_db_store::create_private_dir(&dir.join("leases"))?;
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock"))?;
        file.lock()?;
        Ok(DirLock(file))
    }

    fn record(&self, source: &str) -> Option<TunnelRecord> {
        let text = fs::read_to_string(self.dir(source).join("tunnel.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_record(&self, source: &str, record: &TunnelRecord) -> Result<()> {
        let path = self.dir(source).join("tunnel.json");
        fs::write(&path, serde_json::to_string(record)?)?;
        herdr_db_store::set_private(&path)?;
        Ok(())
    }

    fn write_lease(&self, source: &str, holder: &str, pid: u32) -> Result<()> {
        let path = self.dir(source).join("leases").join(holder);
        fs::write(&path, pid.to_string())?;
        herdr_db_store::set_private(&path)?;
        Ok(())
    }

    /// Removes dead leases and returns the holders still alive.
    fn live_leases(&self, source: &str, probe: &dyn Probe) -> Vec<String> {
        let dir = self.dir(source).join("leases");
        let mut live = Vec::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let pid: Option<u32> = fs::read_to_string(entry.path()).ok().and_then(|t| t.trim().parse().ok());
            match pid {
                Some(pid) if probe.alive(pid) => live.push(entry.file_name().to_string_lossy().into_owned()),
                _ => {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        live.sort();
        live
    }

    fn stop(&self, source: &str) {
        if let Some(record) = self.record(source) {
            tracing::info!(source, pid = record.pid, "stopping tunnel");
            terminate_group(record.pid);
        }
        let _ = fs::remove_file(self.dir(source).join("tunnel.json"));
    }

    /// Blocking: takes or starts the tunnel and records this holder's lease.
    pub fn acquire(&self, spec: &Spec<'_>, holder: &str, holder_pid: u32, probe: &dyn Probe) -> Result<Lease> {
        let _lock = self.lock(spec.source)?;
        self.live_leases(spec.source, probe);
        let record = self.record(spec.source);
        let tunnel_alive = record.as_ref().is_some_and(|r| probe.alive(r.pid));
        let port_open = probe.port_open(spec.host, spec.port);
        match decide(tunnel_alive, port_open) {
            Decision::Reuse => {}
            Decision::PortTaken => bail!(
                "le port {}:{} est déjà occupé par un processus qui n'est pas le tunnel de herdr-db \
                 (port-forward lancé à la main ?) : connexion annulée pour ne pas viser la mauvaise base",
                spec.host,
                spec.port
            ),
            decision => {
                if decision == Decision::Restart {
                    self.stop(spec.source);
                } else if record.is_some() {
                    let _ = fs::remove_file(self.dir(spec.source).join("tunnel.json"));
                }
                let pid = self.start(spec, probe)?;
                self.write_record(
                    spec.source,
                    &TunnelRecord {
                        pid,
                        host: spec.host.to_string(),
                        port: spec.port,
                        command: spec.command.to_vec(),
                        started_at: chrono::Utc::now().timestamp(),
                    },
                )?;
            }
        }
        self.write_lease(spec.source, holder, holder_pid)?;
        Ok(Lease {
            tunnels: self.clone(),
            source: spec.source.to_string(),
            holder: holder.to_string(),
            released: false,
        })
    }

    fn start(&self, spec: &Spec<'_>, probe: &dyn Probe) -> Result<u32> {
        let (program, args) = spec.command.split_first().context("pre_connect vide")?;
        let log_path = self.dir(spec.source).join("tunnel.log");
        let log = File::create(&log_path)?;
        herdr_db_store::set_private(&log_path)?;
        let mut command = Command::new(program);
        command.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().with_context(|| format!("pre_connect : impossible de lancer `{program}`"))?;
        let pid = child.id();
        tracing::info!(source = spec.source, pid, program = %program, "tunnel started");
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if probe.port_open(spec.host, spec.port) {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                let tail = log_tail(&log_path);
                bail!("pre_connect `{program}` s'est arrêté ({status}){tail}");
            }
            if Instant::now() > deadline {
                terminate_group(pid);
                let tail = log_tail(&log_path);
                bail!(
                    "pre_connect : le port {}:{} ne répond pas après {} s{tail}",
                    spec.host,
                    spec.port,
                    START_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Reap the child when it exits so it never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(pid)
    }

    pub fn release(&self, source: &str, holder: &str, probe: &dyn Probe) -> Result<()> {
        let _lock = self.lock(source)?;
        let _ = fs::remove_file(self.dir(source).join("leases").join(holder));
        if self.live_leases(source, probe).is_empty() {
            self.stop(source);
        }
        Ok(())
    }

    /// Startup cleanup: dead leases go, tunnels without live leases stop.
    pub fn collect_garbage(&self, probe: &dyn Probe) -> Vec<String> {
        let mut stopped = Vec::new();
        for entry in fs::read_dir(&self.root).into_iter().flatten().filter_map(|e| e.ok()) {
            let source = entry.file_name().to_string_lossy().into_owned();
            let Ok(_lock) = self.lock(&source) else { continue };
            if self.live_leases(&source, probe).is_empty() && self.record(&source).is_some() {
                self.stop(&source);
                stopped.push(source);
            }
        }
        stopped
    }
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(3).collect();
    if tail.is_empty() {
        String::new()
    } else {
        format!(" : {}", tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
    }
}

/// A pane's hold on a tunnel. Released explicitly or on drop.
pub struct Lease {
    tunnels: Tunnels,
    source: String,
    holder: String,
    released: bool,
}

impl Lease {
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if !self.released {
            self.released = true;
            if let Err(e) = self.tunnels.release(&self.source, &self.holder, &SystemProbe) {
                tracing::warn!(error = %e, source = %self.source, "tunnel release failed");
            }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

pub async fn acquire(
    state_dir: PathBuf,
    source: String,
    command: Vec<String>,
    host: String,
    port: u16,
    holder: String,
) -> Result<Lease> {
    tokio::task::spawn_blocking(move || {
        Tunnels::new(&state_dir).acquire(
            &Spec { source: &source, command: &command, host: &host, port },
            &holder,
            std::process::id(),
            &SystemProbe,
        )
    })
    .await
    .map_err(|e| anyhow!("tunnel : {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    struct FakeProbe {
        alive: RefCell<HashSet<u32>>,
        port: RefCell<bool>,
    }

    impl Probe for FakeProbe {
        fn alive(&self, pid: u32) -> bool {
            self.alive.borrow().contains(&pid)
        }
        fn port_open(&self, _: &str, _: u16) -> bool {
            *self.port.borrow()
        }
    }

    fn probe(alive: &[u32], port: bool) -> FakeProbe {
        FakeProbe { alive: RefCell::new(alive.iter().copied().collect()), port: RefCell::new(port) }
    }

    fn record(tunnels: &Tunnels, pid: u32) {
        herdr_db_store::create_private_dir(&tunnels.dir("db").join("leases")).unwrap();
        tunnels
            .write_record(
                "db",
                &TunnelRecord { pid, host: "localhost".into(), port: 1, command: vec!["x".into()], started_at: 0 },
            )
            .unwrap();
    }

    #[test]
    fn decisions() {
        assert_eq!(decide(true, true), Decision::Reuse);
        assert_eq!(decide(true, false), Decision::Restart);
        assert_eq!(decide(false, true), Decision::PortTaken);
        assert_eq!(decide(false, false), Decision::Start);
    }

    #[test]
    fn live_tunnel_is_shared_and_stopped_by_the_last_lease() {
        let dir = tempfile::tempdir().unwrap();
        let tunnels = Tunnels::new(dir.path());
        // PID 4_000_000 does not exist: terminate_group is harmless.
        record(&tunnels, 4_000_000);
        let probe = probe(&[4_000_000, 11, 12], true);
        let spec = Spec { source: "db", command: &["x".into()], host: "localhost", port: 1 };
        let a = tunnels.acquire(&spec, "pane-a", 11, &probe).unwrap();
        let b = tunnels.acquire(&spec, "pane-b", 12, &probe).unwrap();
        assert_eq!(tunnels.live_leases("db", &probe), vec!["pane-a", "pane-b"]);
        std::mem::forget(a);
        std::mem::forget(b);

        tunnels.release("db", "pane-a", &probe).unwrap();
        assert!(tunnels.record("db").is_some(), "another lease remains");
        tunnels.release("db", "pane-b", &probe).unwrap();
        assert!(tunnels.record("db").is_none(), "last lease stops the tunnel");
    }

    #[test]
    fn dead_leases_do_not_keep_a_tunnel() {
        let dir = tempfile::tempdir().unwrap();
        let tunnels = Tunnels::new(dir.path());
        record(&tunnels, 4_000_001);
        tunnels.write_lease("db", "crashed", 999_999).unwrap();
        let probe = probe(&[4_000_001], true);
        assert_eq!(tunnels.collect_garbage(&probe), vec!["db"]);
        assert!(tunnels.record("db").is_none());
        assert!(tunnels.live_leases("db", &probe).is_empty());
    }

    #[test]
    fn foreign_process_on_the_port_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let tunnels = Tunnels::new(dir.path());
        let probe = probe(&[], true);
        let spec = Spec { source: "db", command: &["x".into()], host: "localhost", port: 1 };
        let error = tunnels.acquire(&spec, "pane", 1, &probe).err().unwrap();
        assert!(error.to_string().contains("déjà occupé"), "{error}");
    }

    #[test]
    fn real_command_is_started_and_awaited() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let tunnels = Tunnels::new(dir.path());
        // A "tunnel" that opens the port after a short delay.
        let script = format!(
            "sleep 0.3; exec python3 -c 'import socket,time; s=socket.socket(); s.bind((\"127.0.0.1\",{port})); s.listen(); time.sleep(30)'"
        );
        let command = vec!["sh".to_string(), "-c".to_string(), script];
        if Command::new("python3").arg("-V").output().is_err() {
            return;
        }
        let spec = Spec { source: "db", command: &command, host: "127.0.0.1", port };
        let lease = tunnels.acquire(&spec, "pane", std::process::id(), &SystemProbe).unwrap();
        let pid = tunnels.record("db").unwrap().pid;
        assert!(pid_alive(pid));
        lease.release();
        std::thread::sleep(Duration::from_millis(300));
        assert!(tunnels.record("db").is_none());
        assert!(!SystemProbe.port_open("127.0.0.1", port), "tunnel stopped with the last lease");
    }
}
