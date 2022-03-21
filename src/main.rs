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

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use clap::Parser;
use futures::StreamExt;
use humantime::Duration as HumanDuration;
use json_patch::{PatchOperation, RemoveOperation, TestOperation};
use k8s_openapi::api::core::v1::Service;
use kube::api::{Patch, PatchParams};
use kube::runtime::finalizer;
use kube::{
    runtime::controller::{Action, Context, Controller},
    runtime::finalizer::{Error as FinalizerError, Event},
    Api, Client, Resource, ResourceExt,
};
use tokio::net::{TcpListener, UdpSocket};
use tracing::{debug, error, info};
use trust_dns_client::rr::{LowerName, RrKey};
use trust_dns_proto::rr::rdata::soa::SOA;
use trust_dns_proto::rr::{Name, RData, Record, RecordSet, RecordType};
use trust_dns_server::authority::{AuthorityObject, Catalog, ZoneType};
use trust_dns_server::store::in_memory::InMemoryAuthority;
use trust_dns_server::ServerFuture;

/// The name of the annotation we use to mark our LoadBalancer objects with the domain name they
/// should be associated with
const HOMELAB_DNS_ANNOTATION: &str = "k8s.r0ssd.co/dns-entry";

/// The label we use configure services with an additional finalizer so we can remove DNS records
const HOMELAB_DNS_FINALIZER: &str = "homelab-dns.k8s.r0ssd.co/cleanup";

/// Implement a custom error type to propagate errors through our controller implementation.
/// NOTE(rossdylan): I would have just used anyhow, but kube-rs needs a `std::error::Error`
/// compatible error type as the result from reconcile
#[derive(thiserror::Error, Debug)]
enum HLDError {
    /// Errors returned from the trust-dns protocol library
    #[error("dns name conversion failure")]
    ProtoError(#[from] trust_dns_proto::error::ProtoError),

    /// An error we kick back when we can't find the namespace of an object
    #[error("no namespace found in object")]
    NoNamespace,

    /// A pass through for kube-rs errors
    #[error("kube-rs failure")]
    KubeError(#[from] kube::Error),

    #[error("finalizer failed to apply")]
    FinalizerApplyFailed {
        #[source]
        source: Box<HLDError>,
    },
    #[error("finalizer cleanup failed")]
    FinalizerCleanupFailed {
        #[source]
        source: Box<HLDError>,
    },
    #[error("failed to add finalizer")]
    FinalizerAddFailed {
        #[source]
        source: kube::Error,
    },
    #[error("failed to add finalizer")]
    FinalizerRemoveFailed {
        #[source]
        source: kube::Error,
    },
    #[error("unnamed object")]
    FinalizerUnnamedObject,
}

impl From<FinalizerError<Self>> for HLDError {
    fn from(error: FinalizerError<Self>) -> Self {
        match error {
            FinalizerError::ApplyFailed(inner) => HLDError::FinalizerApplyFailed {
                source: Box::new(inner),
            },
            FinalizerError::CleanupFailed(inner) => HLDError::FinalizerCleanupFailed {
                source: Box::new(inner),
            },
            FinalizerError::AddFinalizer(inner) => HLDError::FinalizerAddFailed { source: inner },
            FinalizerError::RemoveFinalizer(inner) => {
                HLDError::FinalizerRemoveFailed { source: inner }
            }
            FinalizerError::UnnamedObject => HLDError::FinalizerUnnamedObject,
        }
    }
}

type HLDResult<T, E = HLDError> = std::result::Result<T, E>;

/// External-ish DNS for homelab kubernetes clusters
#[derive(Parser, Debug, Clone)]
#[clap(author = "Ross Delinger <rossdylan@fastmail.com>", version = "0.1", about, long_about = None)]
struct Args {
    /// The root domain we are serving DNS for.
    #[clap(short, long)]
    origin: Name,

    /// Address and port to bind our DNS server on
    #[clap(short, long, default_value = "127.0.0.1:53")]
    bind: SocketAddr,

    /// Timeout for DNS over TCP connections.
    #[clap(long, default_value = "1s")]
    tcp_timeout: HumanDuration,

    /// Set the TTL of the LoadBalancer DNS records
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

/// Create our DNS server and return the underlying zone we use to update entries with
async fn spawn_server(args: &Args) -> anyhow::Result<Arc<InMemoryAuthority>> {
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
    tracing::info!("dns udp bound to {}", udp_sock.local_addr().unwrap());
    let tcp_listener = TcpListener::bind(args.bind).await?;
    tracing::info!("dns tcp bound to {}", tcp_listener.local_addr().unwrap());

    server.register_socket(udp_sock);
    server.register_listener(tcp_listener, *args.tcp_timeout);
    tokio::task::spawn(server.block_until_done());
    Ok(authority)
}

/// State passed to our reconciler function via the kube-rs Controller. It contains a reference to our
/// DNS authority and general application configuration used to create new entries.
struct ReconcilerState {
    authority: Arc<InMemoryAuthority>,
    service_to_domain: Arc<Mutex<HashMap<String, String>>>,
    kube: Client,
    args: Args,
}

impl ReconcilerState {
    fn new(authority: Arc<InMemoryAuthority>, args: Args, kube: Client) -> Self {
        ReconcilerState {
            authority,
            service_to_domain: Arc::new(Mutex::new(HashMap::new())),
            kube,
            args,
        }
    }

    fn domain_for_service(&self, service: &str) -> Option<String> {
        self.service_to_domain
            .lock()
            .unwrap()
            .get(service)
            .map(Clone::clone)
    }

    fn remove_domain_for_service(&self, service: &str) {
        self.service_to_domain.lock().unwrap().remove(service);
    }

    fn set_domain_for_service(&self, service: &str, domain: &str) {
        self.service_to_domain
            .lock()
            .unwrap()
            .insert(service.into(), domain.into());
    }
}

/// Run a single instance of our reconciliation loop. We check to see if the changed service is one we
/// should care about, then use the kube-rs finalizer support to either apply new updates to our
/// authoritative zone, or remove the relevant records.
async fn reconcile(service: Arc<Service>, ctx: Context<ReconcilerState>) -> HLDResult<Action> {
    // The first thing we do is check to see if we care about this Service.
    // This way we don't attach finalizers to all services
    let is_lb = if let Some(spec) = &service.spec {
        matches!(spec.type_.as_deref(), Some("LoadBalancer"))
    } else {
        false
    };
    let lb_name = service.metadata.name.as_deref().unwrap_or("???");
    if !is_lb {
        debug!(
            "ignoring service {} because it is not a LoadBlanacer",
            lb_name
        );
        return Ok(Action::requeue(Duration::from_secs(3600)));
    }
    let state = ctx.get_ref();
    let anno = get_hld_annotation(&service);
    let previous_anno = state.domain_for_service(lb_name);
    let ns = service
        .meta()
        .namespace
        .as_deref()
        .ok_or(HLDError::NoNamespace)?;
    let sapi: Api<Service> = Api::namespaced(ctx.get_ref().kube.clone(), ns);

    if anno.is_none() && previous_anno.is_none() {
        debug!(
            "ignoring service {} because it is not marked with '{}'",
            lb_name, HOMELAB_DNS_ANNOTATION,
        );
        return Ok(Action::requeue(Duration::from_secs(3600)));
    }
    // Handle our early termination case where something has removed the annotation from a service but not deleted it completely
    if anno.is_none() && previous_anno.is_some() {
        return reconcile_early_cleanup(sapi, service, ctx).await;
    }

    let action = finalizer(&sapi, HOMELAB_DNS_FINALIZER, service, |event| async {
        match event {
            Event::Apply(svc) => reconcile_apply(svc, ctx).await,
            Event::Cleanup(svc) => reconcile_cleanup(svc, ctx).await,
        }
    })
    .await?;
    Ok(action)
}

/// This cleanup helper is used to remove DNS entries when something has removed the homelab-dns annotation from a service
async fn reconcile_early_cleanup(
    sapi: Api<Service>,
    service: Arc<Service>,
    ctx: Context<ReconcilerState>,
) -> HLDResult<Action> {
    reconcile_cleanup(service.clone(), ctx).await?;
    let finalizer_index = &service
        .finalizers()
        .iter()
        .enumerate()
        .find(|(_, fin)| *fin == HOMELAB_DNS_FINALIZER)
        .map(|(i, _)| i);
    if let Some(fi) = finalizer_index {
        let name = service
            .meta()
            .name
            .clone()
            .ok_or(HLDError::FinalizerUnnamedObject)?;

        let finalizer_path = format!("/metadata/finalizers/{}", fi);
        sapi.patch::<Service>(
            &name,
            &PatchParams::default(),
            &Patch::Json(json_patch::Patch(vec![
                // All finalizers run concurrently and we use an integer index
                // `Test` ensures that we fail instead of deleting someone else's finalizer
                // (in which case a new `Cleanup` event will be sent)
                PatchOperation::Test(TestOperation {
                    path: finalizer_path.clone(),
                    value: HOMELAB_DNS_FINALIZER.into(),
                }),
                PatchOperation::Remove(RemoveOperation {
                    path: finalizer_path,
                }),
            ])),
        )
        .await
        .map_err(|e| HLDError::FinalizerRemoveFailed { source: e })?;
    }
    Ok(Action::await_change())
}

async fn reconcile_cleanup(
    service: Arc<Service>,
    ctx: Context<ReconcilerState>,
) -> HLDResult<Action> {
    let state = ctx.get_ref();
    let mut records = state.authority.records_mut().await;

    let lb_name = service.metadata.name.as_deref().unwrap_or("???");
    // Grab the domain name from the current annotation, or the last value we observed
    let anno = get_hld_annotation(&service).or_else(|| state.domain_for_service(lb_name));
    if anno.is_none() {
        debug!(
            "ignoring service {} because it is not marked with '{}'",
            lb_name, HOMELAB_DNS_ANNOTATION,
        );
        return Ok(Action::await_change());
    }

    let name = Name::from_str(&anno.unwrap())?;

    let a_key = RrKey::new(LowerName::new(&name), RecordType::A);
    let aaaa_key = RrKey::new(LowerName::new(&name), RecordType::AAAA);
    if let Some(a_records) = records.remove(&a_key) {
        info!(
            "removing A records {:?}",
            a_records.records_without_rrsigs()
        );
    }
    if let Some(aaaa_records) = records.remove(&aaaa_key) {
        info!(
            "removing AAAA records {:?}",
            aaaa_records.records_without_rrsigs()
        );
    }
    state.remove_domain_for_service(lb_name);
    Ok(Action::await_change())
}

/// This function is called when we get a new annotated service and we need to update our
/// DNS records.
async fn reconcile_apply(
    service: Arc<Service>,
    ctx: Context<ReconcilerState>,
) -> HLDResult<Action> {
    let state = ctx.get_ref();
    let anno = get_hld_annotation(&service).unwrap();
    let lb_name = service.metadata.name.as_deref().unwrap_or("???");
    let name = Name::from_str(&anno)?;

    if let Some(addrs) = get_lb_ips(&service) {
        // TODO(rossdylan): We don't handle the case where the assigned addresses have been changed.
        // This will require some deeper twiddling of the authority database to remove old records
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
            let success = state
                .authority
                .upsert(Record::from_rdata(name.clone(), state.args.ttl, rdata), 0)
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
    state.set_domain_for_service(lb_name, &anno);
    Ok(Action::requeue(Duration::from_secs(3600)))
}

fn error_policy(error: &HLDError, _ctx: Context<ReconcilerState>) -> Action {
    error!("reconciliation failed: {}", error);
    Action::requeue(Duration::from_secs(5))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt::init();

    let authority = spawn_server(&args).await?;

    // This part is all trust-dns-server configuration
    // All the kube reflector shit is below this point
    let client = Client::try_default().await?;
    let services: Api<Service> = Api::all(client.clone());

    let state = ReconcilerState::new(authority, args, client);
    Controller::new(services, Default::default())
        .run(reconcile, error_policy, Context::new(state))
        .for_each(|_| futures::future::ready(()))
        .await;
    Ok(())
}
