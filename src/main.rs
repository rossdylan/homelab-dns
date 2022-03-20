//! A simple k8s API watcher that exposes a DNS server for homelab DNS resolution.
//! Ideal for usage with k3s, metallb, and a ubiquiti edgerouter.
//! This system is designed to be as simple as possible while still providing internal only
//! DNS for homelab kubernetes clsuters. There is no support for other kinds of DNS servers as
//! we build our DNS server into the same process as our k8s watcher
#![warn(
    clippy::dbg_macro,
    clippy::unimplemented,
    missing_copy_implementations,
    missing_docs,
    non_snake_case,
    non_upper_case_globals,
    rust_2018_idioms,
    unreachable_pub
)]

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use clap::Parser;
use futures::{pin_mut, TryStreamExt};
use k8s_openapi::api::core::v1::Service;
use kube::{
    api::{Api, ListParams},
    runtime::{reflector, utils::try_flatten_applied, watcher},
    Client,
};
use tokio::net::{TcpListener, UdpSocket};
use tracing::{error, info};
use trust_dns_client::rr::{LowerName, RrKey};
use trust_dns_proto::rr::rdata::soa::SOA;
use trust_dns_proto::rr::{Name, RData, Record, RecordSet, RecordType};
use trust_dns_server::authority::{AuthorityObject, Catalog, ZoneType};
use trust_dns_server::store::in_memory::InMemoryAuthority;
use trust_dns_server::ServerFuture;

/// The name of the annotation we use to mark our LoadBalancer objects with the domain name they
/// should be associated with
const HOMELAB_DNS_ANNOTATION: &str = "k8s.r0ssd.co/dns-entry";

/// External-ish DNS for homelab kubernetes clusters
#[derive(Parser, Debug)]
#[clap(author = "Ross Delinger <rossdylan@fastmail.com>", version = "0.1", about, long_about = None)]
struct Args {
    /// The root domain we are serving DNS for.
    #[clap(short, long)]
    origin: Name,

    /// Address and port to bind our DNS server on
    #[clap(short, long, default_value = "127.0.0.1:53")]
    bind: SocketAddr,

    // Time to set on TCP dns lookups in seconds
    #[clap(long, default_value_t = 1)]
    tcp_timeout_secs: u64,

    #[clap(short, long, default_value_t = 300)]
    ttl: u32,
}

/// Try and extract the IP addresses from the load balancer status
fn get_lb_ips(srv: &Service) -> Option<Vec<String>> {
    if let Some(status) = &srv.status {
        if let Some(lb) = &status.load_balancer {
            if let Some(ingress) = &lb.ingress {
                let ips: Vec<String> = ingress
                    .iter()
                    .filter(|lbi| lbi.ip.is_some())
                    .map(|lbi| lbi.ip.as_ref().unwrap().clone())
                    .collect();
                return Some(ips);
            }
        }
    }
    None
}

/// Extract the annotation used to specify what domain name we should register for this LoadBalancer
fn get_hld_annotation(srv: &Service) -> Option<String> {
    if let Some(annotations) = &srv.metadata.annotations {
        annotations.get(HOMELAB_DNS_ANNOTATION).map(Clone::clone)
    } else {
        None
    }
}

/// Create our initial records. Right now this is just the SOA record but eventually this will
/// pull all LBs from k8s and ensure records are created before we ever start serving
fn generate_initial_records(origin: &Name) -> BTreeMap<RrKey, RecordSet> {
    let mname_prefix = Name::from_str("ns.dns").unwrap();
    let rname_prefix = Name::from_str("hostmaster").unwrap();

    let mut map = BTreeMap::new();
    let mut rr = RecordSet::new(origin, RecordType::SOA, 0);
    let soa = SOA::new(
        mname_prefix.append_domain(origin).unwrap(),
        rname_prefix.append_domain(origin).unwrap(),
        0,
        300,
        300,
        i32::MAX,
        100,
    );
    rr.add_rdata(RData::SOA(soa));
    map.insert(RrKey::new(LowerName::new(origin), RecordType::SOA), rr);
    map
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt::init();

    // This part is all trust-dns-server configuration
    let recs = generate_initial_records(&args.origin);
    let authority = Arc::new(
        InMemoryAuthority::new(args.origin.clone(), recs, ZoneType::Primary, false)
            .map_err(|e| anyhow!(e))?,
    );
    let boxed_authority = Box::new(authority.clone()) as Box<dyn AuthorityObject>;
    let mut catalog = Catalog::new();
    catalog.upsert(LowerName::new(&args.origin), boxed_authority);

    let mut server = ServerFuture::new(catalog);
    let udp_sock = UdpSocket::bind(args.bind).await?;
    let tcp_listener = TcpListener::bind(args.bind).await?;

    server.register_socket(udp_sock);
    server.register_listener(tcp_listener, Duration::from_secs(args.tcp_timeout_secs));
    tokio::task::spawn(server.block_until_done());

    // All the kube reflector shit is below this point
    let client = Client::try_default().await?;
    let services: Api<Service> = Api::all(client);
    let lp = ListParams::default();
    let store = reflector::store::Writer::<Service>::default();
    // TODO(rossdylan): Spin off a background task to read through our state every 15-30s and handle deletions
    let reader = store.as_reader();

    let rf = reflector(store, watcher(services, lp));
    let current = reader.state();
    for service in current {
        info!("initial read of service: {:?}", *service)
    }
    let sw = try_flatten_applied(rf);
    pin_mut!(sw);
    while let Some(service) = sw.try_next().await? {
        let is_lb = if let Some(spec) = &service.spec {
            matches!(spec.type_.as_deref(), Some("LoadBalancer"))
        } else {
            false
        };
        let lb_name = service.metadata.name.as_deref().unwrap_or("???");

        if !is_lb {
            continue;
        }

        let anno = get_hld_annotation(&service);
        if anno.is_none() {
            continue;
        }
        let name = match Name::from_str(&anno.unwrap()) {
            Err(e) => {
                error!("annotated domain has invalid format: {}", e);
                continue;
            }
            Ok(n) => n,
        };

        if let Some(addrs) = get_lb_ips(&service) {
            for addr in addrs {
                let parsed_addr = match addr.parse::<IpAddr>() {
                    Err(e) => {
                        error!("failed to parse IP Address for service {}: {}", lb_name, e);
                        continue;
                    }
                    Ok(pa) => pa,
                };
                let rdata = match parsed_addr {
                    IpAddr::V4(v4) => RData::A(v4),
                    IpAddr::V6(v6) => RData::AAAA(v6),
                };
                let success = authority
                    .upsert(Record::from_rdata(name.clone(), args.ttl, rdata), 0)
                    .await;
                if success {
                    info!(
                        "added record for service {}: {} -> {}",
                        lb_name, name, parsed_addr
                    );
                } else {
                    info!(
                        "failed to add record for service {}: {} -> {}",
                        lb_name, name, parsed_addr
                    );
                }
            }
        }
    }
    Ok(())
}
