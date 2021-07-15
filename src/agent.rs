use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::process;
use std::sync::Arc;
use anyhow::{Error, Result};
use clap::value_t;
use ed25519_compact::{KeyPair, Seed};
use log::{debug, error, info, warn};
use nix::{unistd::gethostname, sys::utsname::uname};
use rustls::RootCertStore;
use rustls_native_certs::load_native_certs;
use signal_hook::{iterator::Signals, {consts::signal::{SIGINT, SIGTERM, SIGUSR1}}};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use trust_dns_resolver::TokioAsyncResolver;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::system_conf::read_system_conf;
use webpki_roots::TLS_SERVER_ROOTS;
use synapi::{Client, Config as ClientConfig, Region};
use netdiag::Bind;
use crate::args::Args;
use crate::exec::Executor;
use crate::export::Exporter;
use crate::output::Output;
use crate::secure;
use crate::status::Monitor;
use crate::task::{Config, Network, Resolver};
use crate::update::Updater;
use crate::version::Version;
use crate::watch::{Event, Watcher};

pub struct Agent {
    bind:    Bind,
    client:  Arc<Client>,
    roots:   RootCertStore,
    events:  Receiver<Event>,
    report:  Sender<Event>,
    watcher: Watcher,
}

impl Agent {
    pub fn new(bind: Bind, client: Arc<Client>, keys: KeyPair, roots: RootCertStore) -> Self {
        let (tx, events) = channel(128);
        let report  = tx.clone();
        let watcher = Watcher::new(client.clone(), keys, tx);
        Self { bind, client, roots, events, report, watcher }
    }

    pub fn report(&self) -> Sender<Event> {
        self.report.clone()
    }

    pub async fn exec(self, exporter: Exporter, net: Option<Network>) -> Result<()> {
        let Self { bind, client, roots, events, watcher, .. } = self;

        let config = Config {
            bind:     bind,
            network:  net,
            resolver: resolver().await?,
            roots:    roots,
        };

        let (tx, mut rx) = channel(16);

        let executor = Executor::new(events, exporter.clone(), config).await?;
        let monitor  = Monitor::new(client, executor.status());

        spawn(monitor.exec(),  tx.clone());
        spawn(watcher.exec(),  tx.clone());
        spawn(exporter.exec(), tx.clone());
        spawn(executor.exec(), tx.clone());

        match rx.recv().await {
            Some(e) => Err(e),
            None    => Ok(()),
        }
    }
}

fn spawn<T: Future<Output = Result<()>> + Send + 'static>(task: T, tx: Sender<Error>) {
    tokio::spawn(async move {
        match task.await {
            Ok(()) => Ok(()),
            Err(e) => tx.send(e).await
        }
    });
}

pub fn agent(args: Args<'_, '_>, version: Version) -> Result<()> {
    let id      = value_t!(args, "id", String)?;
    let name    = args.opt("name")?;
    let global  = args.is_present("global");
    let company = args.opt("company")?;
    let site    = args.opt("site")?;
    let region  = value_t!(args, "region", Region)?;
    let proxy   = args.opt("proxy")?;
    let ip4     = !args.is_present("ip6");
    let ip6     = !args.is_present("ip4");
    let user    = args.value_of("user");
    let update  = args.is_present("update");
    let output  = args.opt("output")?;
    let release = !args.is_present("rc");

    let mut bind = Bind::default();
    if let Some(addrs) = args.values_of("bind") {
        for addr in addrs {
            bind.set(addr.parse()?);
        }
    }

    let name = match name {
        Some(name) => name,
        None       => hostname()?,
    };

    let net = match (ip4, ip6) {
        (true, false) => Some(Network::IPv4),
        (false, true) => Some(Network::IPv6),
        _             => None,
    };

    info!("initializing {} {}", version.name, version.version);

    let keys = match fs::metadata(&id) {
        Ok(_)  => load(&id)?,
        Err(_) => init(&id)?,
    };

    let id = hex::encode(&keys.pk[..6]);
    debug!("name '{}' identity: {}", name, id);

    if let Err(e) = secure::apply(user) {
        error!("agent security failure: {}", e);
    }

    let roots  = trust_roots();
    let client = Arc::new(Client::new(ClientConfig {
        name:    name.clone(),
        global:  global,
        region:  region,
        version: version.version.clone(),
        machine: machine(),
        company: company,
        site:    site,
        proxy:   proxy,
        bind:    args.opt("bind")?,
        roots:   roots.clone(),
    })?);

    let exporter = match output {
        Some(Output::Influx(url, auth)) => Exporter::influx(name, &url, auth)?,
        Some(Output::Kentik) | None     => Exporter::kentik(client.clone())?,
    };

    let runtime = Runtime::new()?;
    let handle  = runtime.handle().clone();
    let agent   = Agent::new(bind, client, keys, roots);
    let report  = agent.report();

    handle.spawn(async move {
        if let Err(e) = agent.exec(exporter, net).await {
            error!("agent failed: {:?}", e);
            process::exit(1);
        }
    });

    let report = move || {
        let report = report.clone();
        handle.spawn(async move {
            match report.send(Event::Report).await {
                Ok(()) => info!("report requested"),
                Err(e) => info!("report error: {:?}", e),
            }
        });
    };

    let updater = Updater::new(version, release, runtime)?;
    let (abort, guard) = updater.exec(update);

    let mut signals = Signals::new(&[SIGINT, SIGTERM, SIGUSR1])?;
    for signal in signals.forever() {
        match signal {
            SIGINT | SIGTERM => break,
            SIGUSR1          => report(),
            _                => unreachable!(),
        }
    }

    abort.abort();
    guard.join().unwrap();

    Ok(())
}

fn load(path: &str) -> Result<KeyPair> {
    let mut file  = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(KeyPair::from_slice(&bytes)?)
}

fn init(path: &str) -> Result<KeyPair> {
    info!("generating new identity");
    let seed = Seed::generate();
    let keys  = KeyPair::from_seed(seed);
    fs::write(path, &keys[..])?;
    Ok(keys)
}

fn hostname() -> Result<String> {
    let mut buf = [0u8; 256];
    let cstr = gethostname(&mut buf)?;
    Ok(cstr.to_string_lossy().to_string())
}

fn machine() -> String {
    let utsname = uname();

    let mut machine = String::new();
    machine.push_str(utsname.sysname());
    machine.push_str(" ");
    machine.push_str(utsname.nodename());
    machine.push_str(" ");
    machine.push_str(utsname.release());
    machine.push_str(" ");
    machine.push_str(utsname.version());
    machine.push_str(" ");
    machine.push_str(utsname.machine());

    machine
}

async fn resolver() -> Result<Resolver> {
    let (config, options) = read_system_conf().unwrap_or_else(|e| {
        warn!("resolver configuration error: {}", e);
        let config  = ResolverConfig::google();
        let options = ResolverOpts::default();
        (config, options)
    });
    Ok(Resolver::new(TokioAsyncResolver::tokio(config, options)?))
}

fn trust_roots() -> RootCertStore {
    let mut store = RootCertStore::empty();

    match load_native_certs() {
        Ok(system)  => store.roots.extend_from_slice(&system.roots),
        Err((_, e)) => warn!("invalid trust store: {}", e),
    };

    if store.roots.is_empty() {
        warn!("using static trust roots");
        store.add_server_trust_anchors(&TLS_SERVER_ROOTS);
    }

    store
}
