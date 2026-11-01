//! craft-a-serverd entry point.
//!
//! `craft-a-serverd [--config PATH]`. Game servers are separate containers: on
//! SIGTERM/SIGINT the daemon stops its control socket and loops and exits, and
//! the servers keep running.

#[cfg(unix)]
use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    app::run()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("craft-a-serverd only runs on Linux (it needs Unix sockets and Podman)");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
mod app {
    use std::os::unix::fs::DirBuilderExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::Duration;

    use craft_a_server_core::config::Config;
    use craft_a_serverd::ctl;
    use craft_a_serverd::db::Db;
    use craft_a_serverd::lifecycle::{Env, Service, TcpProbe};
    use craft_a_serverd::runtime::{ContainerRuntime, PodmanCli};
    use tokio::sync::watch;

    const DEFAULT_CONFIG: &str = "/etc/craft-a-server/config.toml";
    const SUPERVISE_EVERY: Duration = Duration::from_secs(10);
    const SCHEDULE_EVERY: Duration = Duration::from_secs(60);
    const TASK_EXIT_GRACE: Duration = Duration::from_secs(30);

    type Svc = Service<PodmanCli, TcpProbe>;

    #[derive(Debug)]
    struct Args {
        config: PathBuf,
    }

    enum Parsed {
        Run(Args),
        Exit(ExitCode),
    }

    fn parse_args(args: impl Iterator<Item = String>) -> Result<Parsed, String> {
        let mut config = PathBuf::from(DEFAULT_CONFIG);
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" | "-c" => {
                    config = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or("--config needs a path")?;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: craft-a-serverd [--config PATH]\n\
                         default config: {DEFAULT_CONFIG} (a missing file means defaults)"
                    );
                    return Ok(Parsed::Exit(ExitCode::SUCCESS));
                }
                "--version" | "-V" => {
                    println!("craft-a-serverd {}", env!("CARGO_PKG_VERSION"));
                    return Ok(Parsed::Exit(ExitCode::SUCCESS));
                }
                other => return Err(format!("unknown argument '{other}' (try --help)")),
            }
        }
        Ok(Parsed::Run(Args { config }))
    }

    pub fn run() -> ExitCode {
        let args = match parse_args(std::env::args().skip(1)) {
            Ok(Parsed::Run(a)) => a,
            Ok(Parsed::Exit(code)) => return code,
            Err(m) => {
                eprintln!("craft-a-serverd: {m}");
                return ExitCode::from(2);
            }
        };
        init_tracing();
        match run_daemon(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("fatal: {e}");
                ExitCode::FAILURE
            }
        }
    }

    fn init_tracing() {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let builder = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .compact();
        // journald stamps every line itself.
        if std::env::var_os("JOURNAL_STREAM").is_some() {
            builder.without_time().init();
        } else {
            builder.init();
        }
    }

    fn load_config(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::from_toml(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "no config file, using defaults");
                let cfg = Config::default();
                cfg.validate().map_err(|e| e.to_string())?;
                Ok(cfg)
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    fn create_private_dir(path: &Path) -> Result<(), String> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))
    }

    fn run_daemon(args: &Args) -> Result<(), String> {
        let cfg = Arc::new(load_config(&args.config)?);
        create_private_dir(&cfg.data_dir)?;
        create_private_dir(&cfg.servers_dir())?;
        create_private_dir(&cfg.backups_dir())?;
        let db = Db::open(&cfg.db_path()).map_err(|e| format!("cannot open the database: {e}"))?;
        let env = Env::system().map_err(|e| format!("cannot read system information: {e}"))?;
        let rt = Arc::new(PodmanCli::new(cfg.podman_bin.clone()));
        let service: Arc<Svc> = Arc::new(Service::new(
            Arc::clone(&cfg),
            db,
            Arc::clone(&rt),
            Arc::new(TcpProbe),
            env,
        ));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the async runtime: {e}"))?;
        runtime.block_on(daemon(service, rt, cfg))
    }

    async fn daemon(service: Arc<Svc>, rt: Arc<PodmanCli>, cfg: Arc<Config>) -> Result<(), String> {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            "craft-a-serverd starting"
        );
        match rt.version().await {
            Ok(v) => tracing::info!(podman = %v, "container engine found"),
            Err(e) => tracing::warn!(error = %e, "podman is not usable yet; servers cannot start"),
        }
        let to_autostart = match service.reconcile().await {
            Ok(report) => {
                tracing::info!(?report, "reconciled");
                report.to_autostart
            }
            Err(e) => {
                tracing::error!(error = %e, "boot reconciliation failed");
                Vec::new()
            }
        };

        let (stop_tx, stop_rx) = watch::channel(false);
        let mut ctl_task = {
            let (svc, path, mut stop) = (
                Arc::clone(&service),
                cfg.socket_path.clone(),
                stop_rx.clone(),
            );
            tokio::spawn(async move {
                ctl::serve(svc, &path, async move {
                    let _ = stop.wait_for(|v| *v).await;
                })
                .await
            })
        };
        // Servers that were running before a reboot come back by themselves,
        // in the background so the control socket is available meanwhile.
        if !to_autostart.is_empty() {
            let svc = Arc::clone(&service);
            tokio::spawn(async move { svc.autostart(&to_autostart).await });
        }
        let supervisor = spawn_loop(
            &service,
            stop_rx.clone(),
            SUPERVISE_EVERY,
            |svc| async move {
                if let Err(e) = svc.supervise_once().await {
                    tracing::error!(error = %e, "supervisor pass failed");
                }
            },
        );
        let scheduler = spawn_loop(&service, stop_rx, SCHEDULE_EVERY, |svc| async move {
            for (id, outcome) in svc.run_due_backups().await {
                match outcome {
                    Ok(b) => tracing::info!(server = %id, backup = %b.id, "scheduled backup done"),
                    Err(e) => tracing::error!(server = %id, error = %e, "scheduled backup failed"),
                }
            }
        });

        // Either a signal or the control socket dying ends the daemon.
        let early = tokio::select! {
            () = shutdown_signal() => None,
            res = &mut ctl_task => Some(res),
        };
        tracing::info!("shutting down; game servers keep running");
        let _ = stop_tx.send(true);

        let mut failure = None;
        let ctl_result = match early {
            Some(res) => res,
            None => match tokio::time::timeout(TASK_EXIT_GRACE, &mut ctl_task).await {
                Ok(res) => res,
                Err(_) => {
                    ctl_task.abort();
                    Ok(Ok(()))
                }
            },
        };
        match ctl_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failure = Some(format!("control socket: {e}")),
            Err(e) => failure = Some(format!("control socket task failed: {e}")),
        }
        for task in [supervisor, scheduler] {
            if tokio::time::timeout(TASK_EXIT_GRACE, task).await.is_err() {
                tracing::warn!("a background loop did not stop in time");
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Runs `body` every `period` (first run immediately) until told to stop.
    fn spawn_loop<F, Fut>(
        service: &Arc<Svc>,
        mut stop: watch::Receiver<bool>,
        period: Duration,
        body: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: Fn(Arc<Svc>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let service = Arc::clone(service);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => body(Arc::clone(&service)).await,
                    () = async { let _ = stop.wait_for(|v| *v).await; } => return,
                }
            }
        })
    }

    async fn shutdown_signal() {
        use tokio::signal::unix::{SignalKind, signal};
        let (term, int) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        );
        match (term, int) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => tracing::info!("SIGTERM received"),
                    _ = int.recv() => tracing::info!("SIGINT received"),
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!(error = %e, "cannot install signal handlers");
                std::future::pending::<()>().await;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn parse(args: &[&str]) -> Result<Parsed, String> {
            parse_args(args.iter().map(|s| (*s).to_owned()))
        }

        #[test]
        fn config_flag() -> Result<(), String> {
            let Parsed::Run(a) = parse(&["--config", "/tmp/x.toml"])? else {
                return Err("expected run".into());
            };
            assert_eq!(a.config, PathBuf::from("/tmp/x.toml"));
            let Parsed::Run(a) = parse(&[])? else {
                return Err("expected run".into());
            };
            assert_eq!(a.config, PathBuf::from(DEFAULT_CONFIG));
            assert!(parse(&["--config"]).is_err());
            assert!(parse(&["--nope"]).is_err());
            Ok(())
        }

        #[test]
        fn missing_config_means_defaults() -> Result<(), String> {
            let cfg = load_config(Path::new("/nonexistent/craft-a-server/config.toml"))?;
            assert_eq!(cfg, Config::default());
            Ok(())
        }
    }
}
