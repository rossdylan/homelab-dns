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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error as HLDError, Result as HLDResult};

use anyhow::anyhow;
use clap::Parser;
use futures::StreamExt;
use hickory_proto::rr::rdata::soa::SOA;
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordSet, RecordType, RrKey};
use hickory_server::authority::{AuthorityObject, Catalog, ZoneType};
use hickory_server::store::in_memory::InMemoryAuthority;
use hickory_server::ServerFuture;
use humantime::Duration as HumanDuration;
use json_patch::{PatchOperation, RemoveOperation, TestOperation};
use k8s_openapi::api::core::v1::Service;
use kube::api::{Patch, PatchParams};
use kube::runtime::finalizer;
use kube::{
    runtime::controller::{Action, Controller},
    runtime::events::{Event as REvent, EventType, Recorder, Reporter},
    runtime::finalizer::Event,
    Api, Client, Resource, ResourceExt,
};
use tokio::net::{TcpListener, UdpSocket};
use tracing::log::warn;
use tracing::{debug, error, info};

mod error;

/// The name of the annotation we use to mark our LoadBalancer objects with the domain name they
/// should be associated with
const HOMELAB_DNS_ANNOTATION: &str = "k8s.r0ssd.co/dns-entry";
const HOMELAB_DNS_ANAME_ANNOTATION: &str = "k8s.r0ssd.co/cname-entry";

/// The label we use configure services with an additional finalizer so we can remove DNS records
const HOMELAB_DNS_FINALIZER: &str = "homelab-dns.k8s.r0ssd.co/cleanup";

/// External-ish DNS for homelab kubernetes clusters
#[derive(Parser, Debug, Clone)]
#[clap(author = "Ross Delinger <rossdylan@fastmail.com>", about, long_about = None)]
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
/// NOTE(rossdylan): I had this as a fairly deeply nested set of if let statements
/// previously. I rewrote it using optional chaining. I think? its better? maybe?
fn get_lb_ips(srv: &Service) -> Option<Vec<String>> {
    srv.status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .map(|i| {
            i.iter()
                .filter(|lbi| lbi.ip.is_some())
                .map(|lbi| lbi.ip.as_ref().unwrap().clone())
                .collect()
        })
}

/// A struct containing the A and ANAME records from the service annotation
#[derive(Clone, Debug)]
struct AnnotationEntries {
    a_record: String,
    anames: Vec<String>,
}

/// Extract the annotation used to specify what domain name we should register for this LoadBalancer
fn get_hld_annotation(srv: &Service) -> Option<AnnotationEntries> {
    let maybe_a = srv
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(HOMELAB_DNS_ANNOTATION))
        .cloned();
    let anames = srv
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(HOMELAB_DNS_ANAME_ANNOTATION))
        .map(|ca| {
            ca.split(",")
                .map(ToOwned::to_owned)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    maybe_a.map(|ae| AnnotationEntries {
        a_record: ae,
        anames,
    })
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

    let mut rr = RecordSet::new(origin, RecordType::NS, 0);
    rr.add_rdata(RData::NS(hickory_proto::rr::rdata::name::NS(
        Name::from_str("localhost").unwrap(),
    )));
    map.insert(RrKey::new(LowerName::new(origin), RecordType::NS), rr);
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
    tokio::task::spawn(async move { server.block_until_done().await });
    Ok(authority)
}

/// State passed to our reconciler function via the kube-rs Controller. It contains a reference to our
/// DNS authority and general application configuration used to create new entries.
struct ReconcilerState {
    authority: Arc<InMemoryAuthority>,
    service_to_domain: Arc<Mutex<HashMap<String, AnnotationEntries>>>,
    reporter: Reporter,
    kube: Client,
    args: Args,
}

impl ReconcilerState {
    fn new(authority: Arc<InMemoryAuthority>, args: Args, kube: Client) -> Self {
        ReconcilerState {
            authority,
            service_to_domain: Arc::new(Mutex::new(HashMap::new())),
            reporter: Reporter {
                controller: "homelab-dns".into(),
                instance: std::env::var("POD_NAME").ok(),
            },
            kube,
            args,
        }
    }

    fn domain_for_service(&self, service: &str) -> Option<AnnotationEntries> {
        self.service_to_domain.lock().unwrap().get(service).cloned()
    }

    fn remove_domain_for_service(&self, service: &str) {
        self.service_to_domain.lock().unwrap().remove(service);
    }

    fn set_domain_for_service(&self, service: &str, entries: &AnnotationEntries) {
        self.service_to_domain
            .lock()
            .unwrap()
            .insert(service.into(), entries.clone());
    }
}

/// Run a single instance of our reconciliation loop. We check to see if the changed service is one we
/// should care about, then use the kube-rs finalizer support to either apply new updates to our
/// authoritative zone, or remove the relevant records.
async fn reconcile(service: Arc<Service>, ctx: Arc<ReconcilerState>) -> HLDResult<Action> {
    // The first thing we do is check to see if we care about this Service.
    // This way we don't attach finalizers to all services
    let is_lb = if let Some(spec) = &service.spec {
        matches!(spec.type_.as_deref(), Some("LoadBalancer"))
    } else {
        false
    };
    let lb_name = service
        .metadata
        .name
        .as_deref()
        .ok_or(HLDError::FinalizerUnnamedObject)?;
    if !is_lb {
        debug!(
            "ignoring service {} because it is not a LoadBlanacer",
            lb_name
        );
        return Ok(Action::requeue(Duration::from_secs(3600)));
    }

    let anno = get_hld_annotation(&service);
    let previous_anno = ctx.domain_for_service(lb_name);
    let ns = service
        .meta()
        .namespace
        .as_deref()
        .ok_or(HLDError::NoNamespace)?;
    let sapi: Api<Service> = Api::namespaced(ctx.kube.clone(), ns);

    if anno.is_none() && previous_anno.is_none() {
        debug!(
            "ignoring service {} because it is not marked with '{}'",
            lb_name, HOMELAB_DNS_ANNOTATION,
        );
        return Ok(Action::requeue(Duration::from_secs(3600)));
    }
    let recorder = Recorder::new(
        ctx.kube.clone(),
        ctx.reporter.clone(),
        service.object_ref(&()),
    );

    // Handle our early termination case where something has removed the annotation from a service but not deleted it completely
    if anno.is_none() && previous_anno.is_some() {
        return reconcile_early_cleanup(&recorder, sapi, service, ctx).await;
    }

    let action_res = finalizer(&sapi, HOMELAB_DNS_FINALIZER, service, |event| async {
        match event {
            Event::Apply(svc) => reconcile_apply(&recorder, svc, ctx).await,
            Event::Cleanup(svc) => reconcile_cleanup(&recorder, svc, ctx).await,
        }
    })
    .await;
    // Log an error event to kube
    match action_res {
        Err(e) => {
            let event = REvent {
                type_: EventType::Warning,
                action: "error".into(),
                reason: "dnsUpdate".into(),
                note: Some(format!("failed to update dns: {}", e)),
                secondary: None,
            };
            if let Err(e) = recorder.publish(event).await {
                warn!("failed to publish svc event to k8s: {}", e);
            }
            Err(e.into())
        }
        Ok(a) => Ok(a),
    }
}

/// This cleanup helper is used to remove DNS entries when something has removed the homelab-dns annotation from a service
async fn reconcile_early_cleanup(
    recorder: &Recorder,
    sapi: Api<Service>,
    service: Arc<Service>,
    ctx: Arc<ReconcilerState>,
) -> HLDResult<Action> {
    reconcile_cleanup(recorder, service.clone(), ctx).await?;
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
                    path: finalizer_path.clone().try_into().unwrap(),
                    value: HOMELAB_DNS_FINALIZER.into(),
                }),
                PatchOperation::Remove(RemoveOperation {
                    path: finalizer_path.try_into().unwrap(),
                }),
            ])),
        )
        .await
        .map_err(|e| HLDError::FinalizerRemoveFailed { source: e })?;
    }
    Ok(Action::await_change())
}

/// This reconcile function is called when our kube-rs managed finalizer has its cleanup triggered.
/// This is the happy fully managed path and kube-rs handles the full finalizer lifecycle (as opposed
/// to the early-cleanup case where we have to manually remove the finalizer ourselves)
async fn reconcile_cleanup(
    recorder: &Recorder,
    service: Arc<Service>,
    ctx: Arc<ReconcilerState>,
) -> HLDResult<Action> {
    let mut records = ctx.authority.records_mut().await;

    let lb_name = service.metadata.name.as_deref().unwrap_or("???");
    // Grab the domain name from the current annotation, or the last value we observed
    let anno = get_hld_annotation(&service).or_else(|| ctx.domain_for_service(lb_name));
    if anno.is_none() {
        debug!(
            "ignoring service {} because it is not marked with '{}'",
            lb_name, HOMELAB_DNS_ANNOTATION,
        );
        return Ok(Action::await_change());
    }
    let aentries = anno.unwrap();
    // Do ANAMEs first to avoid orphaning them
    for cname in aentries.anames {
        let name = Name::from_str(&cname)?;

        let key = RrKey::new(LowerName::new(&name), RecordType::ANAME);
        if let Some(records) = records.remove(&key) {
            let event = REvent {
                action: "remove".into(),
                reason: "dnsUpdate".into(),
                note: Some(format!("removing dns record '{}'", cname)),
                type_: EventType::Normal,
                secondary: None,
            };
            if let Err(e) = recorder.publish(event).await {
                warn!("failed to publish svc event to k8s: {}", e);
            }
            info!(
                "removing ANAME records {:?}",
                records.records_without_rrsigs()
            );
        }
    }
    // Now remove the backing A/AAAA records
    let name = Name::from_str(&aentries.a_record)?;

    let a_key = RrKey::new(LowerName::new(&name), RecordType::A);
    let aaaa_key = RrKey::new(LowerName::new(&name), RecordType::AAAA);
    if let Some(a_records) = records.remove(&a_key) {
        let event = REvent {
            action: "remove".into(),
            reason: "dnsUpdate".into(),
            note: Some(format!("removing dns record '{}'", name)),
            type_: EventType::Normal,
            secondary: None,
        };
        if let Err(e) = recorder.publish(event).await {
            warn!("failed to publish svc event to k8s: {}", e);
        }
        info!(
            "removing A records {:?}",
            a_records.records_without_rrsigs()
        );
    }
    if let Some(aaaa_records) = records.remove(&aaaa_key) {
        let event = REvent {
            action: "remove".into(),
            reason: "dnsUpdate".into(),
            note: Some(format!("removing dns record '{}'", name)),
            type_: EventType::Normal,
            secondary: None,
        };
        if let Err(e) = recorder.publish(event).await {
            warn!("failed to publish svc event to k8s: {}", e);
        }
        info!(
            "removing AAAA records {:?}",
            aaaa_records.records_without_rrsigs()
        );
    }
    ctx.remove_domain_for_service(lb_name);
    Ok(Action::await_change())
}

/// This function is called when we get a new annotated service and we need to update our
/// DNS records.
async fn reconcile_apply(
    recorder: &Recorder,
    service: Arc<Service>,
    ctx: Arc<ReconcilerState>,
) -> HLDResult<Action> {
    let anno = get_hld_annotation(&service).unwrap();
    let lb_name = service.metadata.name.as_deref().unwrap_or("???");
    let prev_anno = ctx.domain_for_service(lb_name);

    // Set up our A records first
    let name = Name::from_str(&anno.a_record)?;

    let addrs = get_lb_ips(&service).unwrap_or_default();
    let parsed: Vec<IpAddr> = addrs.iter().flat_map(|s| s.parse().ok()).collect();
    // TODO(rossdylan): We don't handle the case where the assigned addresses have been changed.
    // This will require some deeper twiddling of the authority database to remove old records
    for addr in parsed.iter() {
        let rdata = match addr {
            IpAddr::V4(v4) => RData::A((*v4).into()),
            IpAddr::V6(v6) => RData::AAAA((*v6).into()),
        };
        let success = ctx
            .authority
            .upsert(Record::from_rdata(name.clone(), ctx.args.ttl, rdata), 0)
            .await;
        if success {
            let event = REvent {
                action: "add".into(),
                reason: "dnsUpdate".into(),
                note: Some(format!(
                    "adding dns record mapping '{}' to '{}'",
                    name, addr
                )),
                type_: EventType::Normal,
                secondary: None,
            };
            if let Err(e) = recorder.publish(event).await {
                warn!("failed to publish svc event to k8s: {}", e);
            }
            info!("added record for service {}: {} -> {}", lb_name, name, addr);
        }
    }

    let prev_cnames: HashSet<&str> = if let Some(ref prev) = prev_anno {
        prev.anames.iter().map(AsRef::as_ref).collect()
    } else {
        HashSet::new()
    };
    let cur_cnames: HashSet<&str> = anno.anames.iter().map(AsRef::as_ref).collect();

    // things to add is the set of names in cur but not in prev
    for cname in cur_cnames.difference(&prev_cnames) {
        let parsed_cname = Name::from_str(cname)?;
        let rdata = RData::ANAME(hickory_proto::rr::rdata::ANAME(name.clone()));
        let success = ctx
            .authority
            .upsert(
                Record::from_rdata(parsed_cname.clone(), ctx.args.ttl, rdata),
                0,
            )
            .await;

        if success {
            let event = REvent {
                action: "add".into(),
                reason: "dnsUpdate".into(),
                note: Some(format!(
                    "adding dns record mapping '{}' to '{}'",
                    parsed_cname, name
                )),
                type_: EventType::Normal,
                secondary: None,
            };
            if let Err(e) = recorder.publish(event).await {
                warn!("failed to publish svc event to k8s: {}", e);
            }
            info!(
                "added aname for service {}: {} -> {}",
                lb_name, parsed_cname, name
            );
        }
    }
    // things to remove is the set of names in prev but not in cur
    for cname in prev_cnames.difference(&cur_cnames) {
        let parsed_cname = Name::from_str(cname)?;
        let mut records = ctx.authority.records_mut().await;
        let key = RrKey::new(LowerName::new(&parsed_cname), RecordType::ANAME);
        if let Some(records) = records.remove(&key) {
            let event = REvent {
                action: "remove".into(),
                reason: "dnsUpdate".into(),
                note: Some(format!("removing dns record '{}'", cname)),
                type_: EventType::Normal,
                secondary: None,
            };
            if let Err(e) = recorder.publish(event).await {
                warn!("failed to publish svc event to k8s: {}", e);
            }
            info!(
                "removing ANAME records {:?}",
                records.records_without_rrsigs()
            );
        }
    }
    ctx.set_domain_for_service(lb_name, &anno);
    Ok(Action::requeue(Duration::from_secs(3600)))
}

/// This is called when our reconciler errors. For now all it does is log
fn error_policy(_services: Arc<Service>, error: &HLDError, _ctx: Arc<ReconcilerState>) -> Action {
    error!("reconciliation failed: {}", error);
    Action::requeue(Duration::from_secs(5))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt::init();

    // TODO(rossdylan): we should preload our DNS server with state scraped from the k8s api.
    // This way on cold boot we don't have a period of time where we don't have any records.
    // That period is short, as we immediately start the reconcile loop and load all records
    // but lets aim for perfection.
    let authority = spawn_server(&args).await?;

    let client = Client::try_default().await?;
    let services: Api<Service> = Api::all(client.clone());

    let state = ReconcilerState::new(authority, args, client);
    Controller::new(services, Default::default())
        .run(reconcile, error_policy, Arc::new(state))
        .for_each(|_| futures::future::ready(()))
        .await;
    Ok(())
}
