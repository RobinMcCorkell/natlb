use std::{
    net::{Ipv4Addr, SocketAddrV4},
    str::FromStr,
    time::Duration,
};

use anyhow::*;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{
    LoadBalancerIngress, LoadBalancerStatus, Node, Service, ServiceStatus,
};
use kube::{
    api::{ListParams, Patch, PatchParams},
    Api, Client, ResourceExt,
};
use kube_runtime::{controller::ReconcilerAction, finalizer, Controller};
use log::{debug, error, info, warn};
use thiserror::Error;

const REFRESH_DURATION: Duration = Duration::from_secs(60);
const REFRESH_DURATION_ON_ERROR: Duration = Duration::from_secs(10);

const PORTMAP_TTL: Duration = Duration::from_secs(240);
const PORTMAP_DESCRIPTION: &str = "k8s natlb";

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
    s: Service,
    services: &Api<Service>,
    nodes: &Api<Node>,
) -> Result<ReconcilerAction, ReconcileError> {
    info!(
        "Found a load balancer {}@{}",
        s.name(),
        s.namespace().unwrap_or_default()
    );

    let spec = s.spec.as_ref().ok_or(ReconcileError::MissingSpec)?;
    let gateway = igd::aio::search_gateway(igd::SearchOptions::default()).await?;

    let node_ips = nodes
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter_map(|n| n.status)
        .filter_map(|ns| {
            ns.addresses
                .into_iter()
                .filter(|a| a.type_ == "InternalIP")
                .filter_map(|a| Ipv4Addr::from_str(&a.address).ok())
                .next()
        })
        .collect::<Vec<_>>();

    for service_port in &spec.ports {
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

    let external_ip = gateway.get_external_ip().await?;

    info!(
        "Setting load balancer IP for {} to {}",
        s.name(),
        external_ip,
    );
    services
        .patch_status(
            &s.name(),
            &PatchParams::apply("natlb"),
            &Patch::Apply(Service {
                status: Some(ServiceStatus {
                    load_balancer: Some(LoadBalancerStatus {
                        ingress: vec![LoadBalancerIngress {
                            ip: Some(external_ip.to_string()),
                            ..Default::default()
                        }],
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        )
        .await?;

    Ok(ReconcilerAction {
        requeue_after: Some(REFRESH_DURATION),
    })
}

async fn cleanup(s: Service) -> Result<ReconcilerAction, ReconcileError> {
    info!(
        "Cleaning up {}@{}",
        s.name(),
        s.namespace().unwrap_or_default()
    );

    let spec = s.spec.as_ref().ok_or(ReconcileError::MissingSpec)?;
    let gateway = igd::aio::search_gateway(igd::SearchOptions::default()).await?;

    for service_port in &spec.ports {
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

    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

#[actix_rt::main]
async fn main() -> Result<()> {
    env_logger::init();
    let client = Client::try_default().await?;

    let services = Api::<Service>::all(client.clone());
    let lp = ListParams::default();

    Controller::new(services, lp)
        .run(
            |s, _| {
                let services = Api::<Service>::namespaced(client.clone(), &s.namespace().unwrap());
                let nodes = Api::<Node>::all(client.clone());
                async move {
                    let spec = s.spec.as_ref().ok_or(finalizer::Error::ApplyFailed {
                        source: ReconcileError::MissingSpec,
                    })?;
                    let ty = spec.type_.as_ref().ok_or(finalizer::Error::ApplyFailed {
                        source: ReconcileError::MissingSpec,
                    })?;
                    if ty != "LoadBalancer" {
                        return Ok(ReconcilerAction {
                            requeue_after: None,
                        });
                    }

                    finalizer::finalizer(
                        &services,
                        "service.kubernetes.io/load-balancer-cleanup",
                        s,
                        |event| async {
                            match event {
                                finalizer::Event::Apply(s) => apply(s, &services, &nodes).await,
                                finalizer::Event::Cleanup(s) => cleanup(s).await,
                            }
                        },
                    )
                    .await
                }
            },
            |_err, _| ReconcilerAction {
                requeue_after: Some(REFRESH_DURATION_ON_ERROR),
            },
            kube_runtime::controller::Context::new(()),
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
