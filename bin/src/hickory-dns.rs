// Copyright 2015-2018 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! The `hickory-dns` binary for running a DNS server
//!
//! ```text
//! Usage: hickory-dns [options]
//!       hickory-dns (-h | --help | --version)
//!
//! Options:
//!    -q, --quiet             Disable INFO messages, WARN and ERROR will remain
//!    -d, --debug             Turn on DEBUG messages (default is only INFO)
//!    -h, --help              Show this message
//!    -v, --version           Show the version of hickory-dns
//!    -c FILE, --config=FILE  Path to configuration file, default is /etc/named.toml
//!    -z DIR, --zonedir=DIR   Path to the root directory for all zone files, see also config toml
//!    -p PORT, --port=PORT    Override the listening port
//!    --tls-port=PORT         Override the listening port for TLS connections
//! ```

#![recursion_limit = "128"]

use std::{
    fmt,
    io::Error,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
};

use clap::Parser;
#[cfg(feature = "prometheus-metrics")]
use metrics_exporter_prometheus::PrometheusBuilder;
use socket2::{Domain, Socket, Type};
use time::OffsetDateTime;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::{
    net::{TcpListener, UdpSocket},
    runtime,
};
#[cfg(any(feature = "__tls", feature = "__https", feature = "__quic"))]
use tracing::warn;
use tracing::{Event, Level, Subscriber, error, info};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, format},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
};

use hickory_dns::Config;
#[cfg(feature = "__tls")]
use hickory_dns::TlsCertConfig;
use hickory_server::{authority::Catalog, server::ServerFuture};

/// Cli struct for all options managed with clap derive api.
#[derive(Debug, Parser)]
#[clap(name = "Hickory DNS named server", version, about)]
struct Cli {
    /// Test validation of configuration files
    #[clap(long = "validate")]
    validate: bool,

    /// Number of runtime workers, defaults to the number of CPU cores
    #[clap(long = "workers")]
    workers: Option<usize>,

    /// Disable INFO messages, WARN and ERROR will remain
    #[clap(short = 'q', long = "quiet", conflicts_with = "debug")]
    quiet: bool,

    /// Turn on `DEBUG` messages (default is only `INFO`)
    #[clap(short = 'd', long = "debug", conflicts_with = "quiet")]
    debug: bool,

    /// Path to configuration file of named server
    #[clap(
        short = 'c',
        long = "config",
        default_value = "/etc/named.toml",
        value_name = "NAME",
        value_hint=clap::ValueHint::FilePath,
    )]
    config: PathBuf,

    /// Path to the root directory for all zone files,
    /// see also config toml
    #[clap(short = 'z', long = "zonedir", value_name = "DIR", value_hint=clap::ValueHint::DirPath)]
    zonedir: Option<PathBuf>,

    /// Listening port for DNS queries,
    /// overrides any value in config file
    #[clap(short = 'p', long = "port", value_name = "PORT")]
    port: Option<u16>,

    /// Listening port for DNS over TLS queries,
    /// overrides any value in config file
    #[cfg(feature = "__tls")]
    #[clap(long = "tls-port", value_name = "TLS-PORT")]
    tls_port: Option<u16>,

    /// Listening port for DNS over HTTPS queries,
    /// overrides any value in config file
    #[cfg(feature = "__https")]
    #[clap(long = "https-port", value_name = "HTTPS-PORT")]
    https_port: Option<u16>,

    /// Listening port for DNS over QUIC queries,
    /// overrides any value in config file
    #[cfg(feature = "__quic")]
    #[clap(long = "quic-port", value_name = "QUIC-PORT")]
    quic_port: Option<u16>,

    /// Listening socket for Prometheus metrics,
    /// for remote access configure socket as needed (e.g. 0.0.0.0:9000)
    /// overrides any value in config file
    #[cfg(feature = "prometheus-metrics")]
    #[clap(
        long = "prometheus-listen-address",
        value_name = "PROMETHEUS-LISTEN-ADDRESS"
    )]
    prometheus_listen_addr: Option<SocketAddr>,

    /// Disable TCP protocol,
    /// overrides any value in config file
    #[clap(long = "disable-tcp")]
    disable_tcp: bool,

    /// Disable UDP protocol,
    /// overrides any value in config file
    #[clap(long = "disable-udp")]
    disable_udp: bool,

    /// Disable TLS protocol,
    /// overrides any value in config file
    #[cfg(feature = "__tls")]
    #[clap(long = "disable-tls", conflicts_with = "tls_port")]
    disable_tls: bool,

    /// Disable HTTPS protocol,
    /// overrides any value in config file
    #[cfg(feature = "__https")]
    #[clap(long = "disable-https", conflicts_with = "https_port")]
    disable_https: bool,

    /// Disable QUIC protocol,
    /// overrides any value in config file
    #[cfg(feature = "__quic")]
    #[clap(long = "disable-quic", conflicts_with = "quic_port")]
    disable_quic: bool,

    /// Disable Prometheus metrics,
    /// overrides any value in config file
    #[cfg(feature = "prometheus-metrics")]
    #[clap(long = "disable-prometheus", conflicts_with = "prometheus_listen_addr")]
    disable_prometheus: bool,
}

/// Main method for running the named server.
fn main() -> Result<(), String> {
    // this is essential for custom formatting the returned error message.
    // the displayed message of termination impl trait is not pretty.
    // https://doc.rust-lang.org/stable/src/std/process.rs.html#2439
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let args = Cli::parse();

    // TODO: this should be set after loading config, but it's necessary for initial log lines, no?
    let level = match (args.quiet, args.debug) {
        (true, _) => Level::ERROR,
        (_, true) => Level::DEBUG,
        _ => Level::INFO,
    };

    // Setup tracing for logging based on input
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().event_format(TdnsFormatter))
        .with(
            EnvFilter::builder()
                .with_default_directive(level.into())
                .from_env()
                .map_err(|err| {
                    format!("failed to parse environment variable for tracing: {err}")
                })?,
        )
        .init();

    info!("Hickory DNS {} starting...", hickory_client::version());

    let mut runtime = runtime::Builder::new_multi_thread();
    runtime.enable_all().thread_name("hickory-server-runtime");
    if let Some(workers) = args.workers {
        runtime.worker_threads(workers);
    }
    let runtime = runtime
        .build()
        .map_err(|err| format!("failed to initialize Tokio runtime: {err}"))?;

    runtime.block_on(async_run(args))
}

async fn async_run(args: Cli) -> Result<(), String> {
    // Load configuration files

    let config = args.config.clone();
    let config_path = Path::new(&config);

    info!("loading configuration from: {config_path:?}");

    let config = Config::read_config(config_path)
        .map_err(|err| format!("failed to read config file from {config_path:?}: {err}"))?;
    let directory_config = config.directory().to_path_buf();
    let zonedir = args.zonedir.clone();
    let zone_dir: PathBuf = zonedir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or(directory_config);

    #[cfg(feature = "prometheus-metrics")]
    if !args.disable_prometheus && !config.disable_prometheus() {
        let socket = args
            .prometheus_listen_addr
            .unwrap_or(config.prometheus_listen_addr());

        // setup tracing/metrics integration and prometheus endpoint
        // execute setup on the existing tokio runtime to ensure that no new runtime is spawned
        // prepare prometheus endpoint
        let prometheus = PrometheusBuilder::new();
        prometheus
            .with_http_listener(socket)
            // either executes on the endpoint on the current tokio runtime or launches a new one
            .install()
            .map_err(|e| format!("failed to install prometheus endpoint {e}"))?;
    } else {
        info!("Prometheus metrics are disabled");
    }

    #[cfg(unix)]
    let mut signal = signal(SignalKind::terminate())
        .map_err(|e| format!("failed to register signal handler: {e}"))?;

    let mut catalog: Catalog = Catalog::new();
    // configure our server based on the config_path
    for zone in config.zones() {
        let zone_name = zone
            .zone()
            .map_err(|err| format!("failed to read zone name from {config_path:?}: {err}"))?;

        match zone.load(&zone_dir).await {
            Ok(authority) => catalog.upsert(zone_name.into(), authority),
            Err(err) => return Err(format!("could not load zone {zone_name}: {err}")),
        }
    }

    let v4addr = config
        .listen_addrs_ipv4()
        .map_err(|err| format!("failed to parse IPv4 addresses from {config_path:?}: {err}"))?;
    let v6addr = config
        .listen_addrs_ipv6()
        .map_err(|err| format!("failed to parse IPv6 addresses from {config_path:?}: {err}"))?;
    let mut listen_addrs: Vec<IpAddr> = v4addr
        .into_iter()
        .map(IpAddr::V4)
        .chain(v6addr.into_iter().map(IpAddr::V6))
        .collect();

    let listen_port: u16 = args.port.unwrap_or_else(|| config.listen_port());

    if listen_addrs.is_empty() {
        listen_addrs.push(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        listen_addrs.push(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    if args.validate {
        info!("configuration files are validated");
        return Ok(());
    }

    let deny_networks = config.deny_networks();
    let allow_networks = config.allow_networks();
    let tcp_request_timeout = config.tcp_request_timeout();

    // now, run the server, based on the config
    #[cfg_attr(not(feature = "__tls"), allow(unused_mut))]
    let mut server = ServerFuture::with_access(catalog, deny_networks, allow_networks);

    if !args.disable_udp && !config.disable_udp() {
        // load all udp listeners
        for addr in &listen_addrs {
            info!("binding UDP to {addr:?}");

            let udp_socket = build_udp_socket(*addr, listen_port)
                .map_err(|err| format!("failed to bind to UDP socket address {addr:?}: {err}"))?;

            info!(
                "listening for UDP on {:?}",
                udp_socket
                    .local_addr()
                    .map_err(|err| format!("failed to lookup local address: {err}"))?
            );

            server.register_socket(udp_socket);
        }
    } else {
        info!("UDP protocol is disabled");
    }

    if !args.disable_tcp && !config.disable_tcp() {
        // load all tcp listeners
        for addr in &listen_addrs {
            info!("binding TCP to {addr:?}");

            let tcp_listener = build_tcp_listener(*addr, listen_port)
                .map_err(|err| format!("failed to bind to TCP socket address {addr:?}: {err}"))?;

            info!(
                "listening for TCP on {:?}",
                tcp_listener
                    .local_addr()
                    .map_err(|err| format!("failed to lookup local address: {err}"))?
            );

            server.register_listener(tcp_listener, tcp_request_timeout);
        }
    } else {
        info!("TCP protocol is disabled");
    }

    #[cfg(feature = "__tls")]
    if let Some(tls_cert_config) = config.tls_cert() {
        #[cfg(feature = "__tls")]
        if !args.disable_tls && !config.disable_tls() {
            // setup TLS listeners
            config_tls(
                args.tls_port,
                &mut server,
                &config,
                tls_cert_config,
                &zone_dir,
                &listen_addrs,
            )?;
        } else {
            info!("TLS protocol is disabled");
        }

        #[cfg(feature = "__https")]
        if !args.disable_https && !config.disable_https() {
            // setup HTTPS listeners
            config_https(
                args.https_port,
                &mut server,
                &config,
                tls_cert_config,
                &zone_dir,
                &listen_addrs,
            )?;
        } else {
            info!("HTTPS protocol is disabled");
        }

        #[cfg(feature = "__quic")]
        if !args.disable_quic && !config.disable_quic() {
            // setup QUIC listeners
            config_quic(
                args.quic_port,
                &mut server,
                &config,
                tls_cert_config,
                &zone_dir,
                &listen_addrs,
            )?;
        } else {
            info!("QUIC protocol is disabled");
        }
    } else {
        info!("TLS certificates are not provided");
        info!("TLS related protocols (TLS, HTTPS and QUIC) are disabled")
    }

    // Drop privileges on Unix systems if running as root.
    #[cfg(target_family = "unix")]
    check_drop_privs(
        config.user.as_deref().unwrap_or(DEFAULT_USER),
        config.group.as_deref().unwrap_or(DEFAULT_GROUP),
    )?;
    #[cfg(not(target_family = "unix"))]
    if config.user.is_some() || config.group.is_some() {
        return Err("dropping privileges is only supported on Unix systems".to_string());
    }

    #[cfg(unix)]
    {
        let token = server.shutdown_token().clone();
        tokio::spawn(async move {
            signal.recv().await;
            token.cancel();
        });
    }

    // config complete, starting!
    banner();

    // TODO: how to do threads? should we do a bunch of listener threads and then query threads?
    // Ideally the processing would be n-threads for receiving, which hand off to m-threads for
    //  request handling. It would generally be the case that n <= m.
    info!("server starting up, awaiting connections...");
    match server.block_until_done().await {
        Ok(()) => {
            // we're exiting for some reason...
            info!("Hickory DNS {} stopping", hickory_client::version());
        }
        Err(e) => {
            let error_msg = format!(
                "Hickory DNS {} has encountered an error: {}",
                hickory_client::version(),
                e
            );

            error!("{}", error_msg);
            panic!("{}", error_msg);
        }
    };

    Ok(())
}

#[cfg(feature = "__tls")]
fn config_tls(
    tls_port: Option<u16>,
    server: &mut ServerFuture<Catalog>,
    config: &Config,
    tls_cert_config: &TlsCertConfig,
    zone_dir: &Path,
    listen_addrs: &[IpAddr],
) -> Result<(), String> {
    let tls_listen_port = tls_port.unwrap_or_else(|| config.tls_listen_port());

    if listen_addrs.is_empty() {
        warn!("a tls certificate was specified, but no TLS addresses configured to listen on");
        return Ok(());
    }

    for addr in listen_addrs {
        let tls_cert_path = &tls_cert_config.path;
        info!("loading cert for DNS over TLS: {tls_cert_path:?}");

        let tls_cert = tls_cert_config.load(zone_dir).map_err(|err| {
            format!("failed to load tls certificate files from {tls_cert_path:?}: {err}")
        })?;

        info!("binding TLS to {addr:?}");

        let tls_listener = build_tcp_listener(*addr, tls_listen_port)
            .map_err(|err| format!("failed to bind to TLS socket address {addr:?}: {err}"))?;

        info!(
            "listening for TLS on {:?}",
            tls_listener
                .local_addr()
                .map_err(|err| format!("failed to lookup local address: {err}"))?
        );

        server
            .register_tls_listener(tls_listener, config.tcp_request_timeout(), tls_cert)
            .map_err(|err| format!("failed to register TLS listener: {err}"))?;
    }
    Ok(())
}

#[cfg(feature = "__https")]
fn config_https(
    https_port: Option<u16>,
    server: &mut ServerFuture<Catalog>,
    config: &Config,
    tls_cert_config: &TlsCertConfig,
    zone_dir: &Path,
    listen_addrs: &[IpAddr],
) -> Result<(), String> {
    let https_listen_port = https_port.unwrap_or_else(|| config.https_listen_port());
    let endpoint_path = config.http_endpoint();

    if listen_addrs.is_empty() {
        warn!("a tls certificate was specified, but no HTTPS addresses configured to listen on");
        return Ok(());
    }

    for addr in listen_addrs {
        let tls_cert_path = &tls_cert_config.path;
        if let Some(endpoint_name) = &tls_cert_config.endpoint_name {
            info!("loading cert for DNS over TLS named {endpoint_name} from {tls_cert_path:?}");
        } else {
            info!("loading cert for DNS over TLS from {tls_cert_path:?}");
        }
        // TODO: see about modifying native_tls to impl Clone for Pkcs12
        let tls_cert = tls_cert_config.load(zone_dir).map_err(|err| {
            format!("failed to load tls certificate files from {tls_cert_path:?}: {err}")
        })?;

        info!("binding HTTPS to {addr:?}");

        let https_listener = build_tcp_listener(*addr, https_listen_port)
            .map_err(|err| format!("failed to bind to HTTPS socket address {addr:?}: {err}"))?;

        info!(
            "listening for HTTPS on {:?}",
            https_listener
                .local_addr()
                .map_err(|err| format!("failed to lookup local address: {err}"))?
        );

        server
            .register_https_listener(
                https_listener,
                config.tcp_request_timeout(),
                tls_cert,
                tls_cert_config.endpoint_name.clone(),
                endpoint_path.into(),
            )
            .map_err(|err| format!("failed to register HTTPS listener: {err}"))?;
    }

    Ok(())
}

#[cfg(feature = "__quic")]
fn config_quic(
    quic_port: Option<u16>,
    server: &mut ServerFuture<Catalog>,
    config: &Config,
    tls_cert_config: &TlsCertConfig,
    zone_dir: &Path,
    listen_addrs: &[IpAddr],
) -> Result<(), String> {
    let quic_listen_port = quic_port.unwrap_or_else(|| config.quic_listen_port());

    if listen_addrs.is_empty() {
        warn!("a tls certificate was specified, but no QUIC addresses configured to listen on");
        return Ok(());
    }

    for addr in listen_addrs {
        let tls_cert_path = &tls_cert_config.path;
        if let Some(endpoint_name) = &tls_cert_config.endpoint_name {
            info!("loading cert for DNS over QUIC named {endpoint_name} from {tls_cert_path:?}");
        } else {
            info!("loading cert for DNS over QUIC from {tls_cert_path:?}",);
        }
        // TODO: see about modifying native_tls to impl Clone for Pkcs12
        let tls_cert = tls_cert_config.load(zone_dir).map_err(|err| {
            format!("failed to load tls certificate files from {tls_cert_path:?}: {err}")
        })?;

        info!("Binding QUIC to {addr:?}");

        let quic_listener = build_udp_socket(*addr, quic_listen_port)
            .map_err(|err| format!("failed to bind to QUIC socket address {addr:?}: {err}"))?;

        info!(
            "listening for QUIC on {:?}",
            quic_listener
                .local_addr()
                .map_err(|err| format!("failed to lookup local address: {err}"))?
        );

        server
            .register_quic_listener(
                quic_listener,
                config.tcp_request_timeout(),
                tls_cert,
                tls_cert_config.endpoint_name.clone(),
            )
            .map_err(|err| format!("failed to register QUIC listener: {err}"))?;
    }
    Ok(())
}

fn banner() {
    #[cfg(feature = "ascii-art")]
    const HICKORY_DNS_LOGO: &str = include_str!("hickory-dns.ascii");

    #[cfg(not(feature = "ascii-art"))]
    const HICKORY_DNS_LOGO: &str = "Hickory DNS";

    info!("");
    for line in HICKORY_DNS_LOGO.lines() {
        info!(" {line}");
    }
    info!("");
}

struct TdnsFormatter;

impl<S, N> FormatEvent<S, N> for TdnsFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let now = OffsetDateTime::now_utc();
        let now_secs = now.unix_timestamp();

        // Format values from the event's's metadata:
        let metadata = event.metadata();
        write!(
            &mut writer,
            "{}:{}:{}",
            now_secs,
            metadata.level(),
            metadata.target()
        )?;

        if let Some(line) = metadata.line() {
            write!(&mut writer, ":{line}")?;
        }

        // Format all the spans in the event's span context.
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                write!(writer, ":{}", span.name())?;

                let ext = span.extensions();
                let fields = &ext
                    .get::<FormattedFields<N>>()
                    .expect("will never be `None`");

                // Skip formatting the fields if the span had no fields.
                if !fields.is_empty() {
                    write!(writer, "{{{fields}}}")?;
                }
            }
        }

        // Write fields on the event
        write!(writer, ":")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

/// Build a TcpListener for a given IP, port pair; IPv6 listeners will not accept v4 connections
fn build_tcp_listener(ip: IpAddr, port: u16) -> Result<TcpListener, Error> {
    let sock = if ip.is_ipv4() {
        Socket::new(Domain::IPV4, Type::STREAM, None)?
    } else {
        let s = Socket::new(Domain::IPV6, Type::STREAM, None)?;
        s.set_only_v6(true)?;
        s
    };

    sock.set_nonblocking(true)?;

    let s_addr = SocketAddr::new(ip, port);
    sock.bind(&s_addr.into())?;

    // this is a fairly typical backlog value, but we don't have any good data to support it as of yet
    sock.listen(128)?;

    TcpListener::from_std(sock.into())
}

/// Build a UdpSocket for a given IP, port pair; IPv6 sockets will not accept v4 connections
fn build_udp_socket(ip: IpAddr, port: u16) -> Result<UdpSocket, Error> {
    let sock = if ip.is_ipv4() {
        Socket::new(Domain::IPV4, Type::DGRAM, None)?
    } else {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, None)?;
        s.set_only_v6(true)?;
        s
    };

    sock.set_nonblocking(true)?;

    let s_addr = SocketAddr::new(ip, port);
    sock.bind(&s_addr.into())?;

    UdpSocket::from_std(sock.into())
}

/// Drop privileges on Unix systems if running as root. Errors that prevent dropping privileges will
/// halt the server.  This must be called after binding to low numbered sockets is complete.
#[cfg(target_family = "unix")]
fn check_drop_privs(user: &str, group: &str) -> Result<(), String> {
    use libc::{getegid, geteuid, getgid, getgrnam, getpwnam, getuid, setgid, setuid};
    use std::ffi::CString;

    // These calls are guaranteed to succeed in a POSIX-conforming environment. In non-conforming
    // environments, implementations may return -1 to indicate a process running without an
    // associated UID/EUID/GID/EGID. In that case, our main block below will not execute as
    // libc typedefs uid_t and gid_t to u32; -1 will be u32::MAX.
    //
    // POSIX reference: IEEE Std 1003.1-1024 getuid, geteuid, getgid, and getegid specifications
    // https://pubs.opengroup.org/onlinepubs/9799919799/functions/getuid.html
    // https://pubs.opengroup.org/onlinepubs/9799919799/functions/geteuid.html
    // https://pubs.opengroup.org/onlinepubs/9799919799/functions/getgid.html
    // https://pubs.opengroup.org/onlinepubs/9799919799/functions/getegid.html
    let (uid, gid, euid, egid) = unsafe { (getuid(), getgid(), geteuid(), getegid()) };

    if uid == 0 || euid == 0 {
        info!(
            "running as root (uid: {uid} gid: {gid} euid: {euid} egid: {egid})...dropping privileges.",
        );

        let Ok(user_cstring) = CString::new(user) else {
            return Err(format!("unable to create CString for user {user}"));
        };

        let Ok(group_cstring) = CString::new(group) else {
            return Err(format!(
                "unable to create CString for group {group}. Exiting."
            ));
        };

        // These functions must be supplied a NULL-terminated string, which is guaranteed by
        // std::ffi::CString.  Upon success, they will return a pointer to a struct passwd or
        // struct group, or NULL upon failure. Testing for a NULL return value is mandatory.
        //
        // POSIX reference: IEEE Std 1003.1-1024 getpwnam and getgrnam specifications
        // https://pubs.opengroup.org/onlinepubs/9799919799/functions/getpwnam.html
        // https://pubs.opengroup.org/onlinepubs/9799919799/functions/getgrnam.html
        let (user_info, group_info) = unsafe {
            (
                getpwnam(user_cstring.as_ptr()),
                getgrnam(group_cstring.as_ptr()),
            )
        };

        if user_info.is_null() {
            return Err(format!("unable to lookup user '{user}'. Exiting."));
        }

        if group_info.is_null() {
            return Err(format!("unable to lookup group '{group}'. Exiting."));
        }

        // These functions must be supplied a gid_t (setgid) and uid_t (setuid), which are
        // supplied by the passwd and group structs returned by getpwnam and getgrnam.
        // The structs are tested to be valid by the calls to is_null() above.
        //
        // The call to setgid must be completed before the call to setuid is made or the
        // process will almost certainly lack the privileges necessary to switch its real gid.
        //
        // POSIX reference: IEEE Std 1003.1-1024 setgid and setuid specifications
        // https://pubs.opengroup.org/onlinepubs/9799919799/functions/setgid.html
        // https://pubs.opengroup.org/onlinepubs/9799919799/functions/setuid.html
        let (setgid_rc, setuid_rc) =
            unsafe { (setgid((*group_info).gr_gid), setuid((*user_info).pw_uid)) };

        if setgid_rc < 0 {
            return Err("unable to set gid. Exiting.".into());
        }

        if setuid_rc < 0 {
            return Err("unable to set uid. Exiting.".into());
        }
    }

    let (uid, gid, euid, egid) = unsafe { (getuid(), getgid(), geteuid(), getegid()) };

    info!("now running as uid: {uid}, gid: {gid} (euid: {euid}, egid: {egid})",);
    Ok(())
}

#[cfg(target_family = "unix")]
static DEFAULT_USER: &str = "nobody";
#[cfg(target_family = "unix")]
static DEFAULT_GROUP: &str = "nobody";
