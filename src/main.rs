use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
    time::Duration,
};

use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    LoadBalancerIngress, LoadBalancerStatus, Node, Service, ServiceStatus,
};
use kube::{
    api::{ListParams, Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher, Controller},
    Api, Client, ResourceExt,
};
use log::{debug, error, info, warn};
use std::sync::Arc;
use thiserror::Error;

const REFRESH_DURATION: Duration = Duration::from_secs(60);
const REFRESH_DURATION_ON_ERROR: Duration = Duration::from_secs(10);

const PORTMAP_TTL: Duration = Duration::from_secs(240);
const PORTMAP_DESCRIPTION: &str = "k8s natlb";

const PORT_FORWARD_ANNOTATION: &str = "natlb.mccorkell.me.uk/port-forward";
const LOAD_BALANCER_CLASS: &str = "natlb.mccorkell.me.uk/natlb";

enum PortForwardMode {
    Invalid(String),
    Upnp,
    Manual,
}

impl From<&Service> for PortForwardMode {
    fn from(svc: &Service) -> Self {
        let annotations = svc.annotations();
        use PortForwardMode::*;
        match annotations.get(PORT_FORWARD_ANNOTATION).map(|s| s.as_str()) {
            None => Upnp,
            Some("upnp") => Upnp,
            Some("manual") => Manual,
            Some(s) => Invalid(s.to_owned()),
        }
    }
}

#[derive(Error, Debug)]
enum ReconcileError {
    #[error("missing object spec")]
    MissingSpec,
    #[error("bad port protocol for port {0:?}")]
    BadPortProtocol(k8s_openapi::api::core::v1::ServicePort),
    #[error("missing node port for port {0:?}")]
    MissingNodePort(k8s_openapi::api::core::v1::ServicePort),
    #[error("unable to build local addr for port {0:?}")]
    BuildLocalAddr(k8s_openapi::api::core::v1::ServicePort),
    #[error("bad port forward mode {0}")]
    BadPortForwardMode(String),

    #[error(transparent)]
    UpnpSearchError(#[from] igd::SearchError),
    #[error(transparent)]
    UpnpAddPortError(#[from] igd::AddPortError),
    #[error(transparent)]
    UpnpRemovePortError(#[from] igd::RemovePortError),
    #[error(transparent)]
    UpnpGetExternalIPError(#[from] igd::GetExternalIpError),

    #[error(transparent)]
    KubeError(#[from] kube::Error),
}

async fn apply(
    s: &Service,
    services: &Api<Service>,
    nodes: &Api<Node>,
) -> Result<Action, ReconcileError> {
    info!(
        "Found a load balancer {}@{}",
        s.name_any(),
        s.namespace().unwrap_or_default(),
    );

    let port_forward_mode = PortForwardMode::from(s);

    let spec = s.spec.as_ref().ok_or(ReconcileError::MissingSpec)?;
    let gateway = igd::aio::search_gateway(igd::SearchOptions::default()).await?;

    match port_forward_mode {
        PortForwardMode::Invalid(s) => return Err(ReconcileError::BadPortForwardMode(s)),
        PortForwardMode::Manual => {
            info!("Manual port forwarding mode");
        }
        PortForwardMode::Upnp => {
            let node_ips = nodes
                .list(&ListParams::default())
                .await?
                .into_iter()
                .filter_map(|n| n.status)
                .filter_map(|ns| {
                    ns.addresses.and_then(|addresses| {
                        addresses
                            .into_iter()
                            .filter(|a| a.type_ == "InternalIP")
                            .filter_map(|a| Ipv4Addr::from_str(&a.address).ok())
                            .next()
                    })
                })
                .collect::<Vec<_>>();

            for service_port in spec.ports.as_ref().unwrap_or(&vec![]) {
                let protocol = match service_port.protocol.as_deref() {
                    Some("TCP") => igd::PortMappingProtocol::TCP,
                    _ => return Err(ReconcileError::BadPortProtocol(service_port.clone())),
                };

                // TODO pick node IPs in a smarter way, e.g. consistent hashing.
                let node_port = service_port
                    .node_port
                    .ok_or_else(|| ReconcileError::MissingNodePort(service_port.clone()))?
                    as u16;
                let node_ip = node_ips
                    .iter()
                    .min()
                    .ok_or_else(|| ReconcileError::BuildLocalAddr(service_port.clone()))?;
                let local_addr = SocketAddrV4::new(*node_ip, node_port);

                info!(
                    "Adding port redirection {} -> {}",
                    service_port.port, local_addr
                );
                gateway
                    .add_port(
                        protocol,
                        service_port.port as u16,
                        local_addr,
                        PORTMAP_TTL.as_secs() as u32,
                        PORTMAP_DESCRIPTION,
                    )
                    .await?;
            }
        }
    }

    let external_ip = gateway.get_external_ip().await?;

    info!(
        "Setting load balancer IP for {} to {}",
        s.name_any(),
        external_ip,
    );
    services
        .patch_status(
            &s.name_any(),
            &PatchParams::apply("natlb"),
            &Patch::Apply(Service {
                status: Some(ServiceStatus {
                    load_balancer: Some(LoadBalancerStatus {
                        ingress: Some(vec![LoadBalancerIngress {
                            ip: Some(external_ip.to_string()),
                            ..Default::default()
                        }]),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        )
        .await?;

    Ok(Action::requeue(REFRESH_DURATION))
}

async fn cleanup(s: &Service) -> Result<Action, ReconcileError> {
    info!(
        "Cleaning up {}@{}",
        s.name_any(),
        s.namespace().unwrap_or_default()
    );

    let spec = s.spec.as_ref().ok_or(ReconcileError::MissingSpec)?;
    let gateway = igd::aio::search_gateway(igd::SearchOptions::default()).await?;

    for service_port in spec.ports.as_ref().unwrap_or(&vec![]) {
        let protocol = match service_port.protocol.as_deref() {
            Some("TCP") => igd::PortMappingProtocol::TCP,
            _ => return Err(ReconcileError::BadPortProtocol(service_port.clone())),
        };
        match gateway
            .remove_port(protocol, service_port.port as u16)
            .await
        {
            Err(igd::RemovePortError::NoSuchPortMapping) => {
                warn!("Port mapping already removed for {:?}", service_port);
                Ok(())
            }
            x => x,
        }?;
    }

    Ok(Action::await_change())
}

#[actix_rt::main]
async fn main() -> Result<()> {
    env_logger::init();
    let client = Client::try_default().await?;

    let services = Api::<Service>::all(client.clone());

    let context = Arc::new(());
    Controller::new(services, watcher::Config::default())
        .run(
            |s, _| {
                let services = Api::<Service>::namespaced(client.clone(), &s.namespace().unwrap());
                let nodes = Api::<Node>::all(client.clone());
                async move {
                    let spec = s
                        .spec
                        .as_ref()
                        .ok_or(finalizer::Error::ApplyFailed(ReconcileError::MissingSpec))?;
                    let ty = spec
                        .type_
                        .as_ref()
                        .ok_or(finalizer::Error::ApplyFailed(ReconcileError::MissingSpec))?;
                    if ty != "LoadBalancer" {
                        return Ok(Action::await_change());
                    }
                    if let Some(class) = spec.load_balancer_class.as_ref() {
                        if class != LOAD_BALANCER_CLASS {
                            return Ok(Action::await_change());
                        }
                    }

                    finalizer::finalizer(
                        &services,
                        "service.kubernetes.io/load-balancer-cleanup",
                        s,
                        |event| async {
                            match event {
                                finalizer::Event::Apply(s) => apply(&s, &services, &nodes).await,
                                finalizer::Event::Cleanup(s) => cleanup(&s).await,
                            }
                        },
                    )
                    .await
                }
            },
            |_obj, _err, _| Action::requeue(REFRESH_DURATION_ON_ERROR),
            context,
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => debug!("reconciled {:?}", o),
                Err(e) => error!("reconcile failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}
