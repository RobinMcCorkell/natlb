use std::time::Duration;

use anyhow::*;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Service;
use kube::{api::ListParams, Api, Client};
use kube_runtime::{
    controller::{Context, ReconcilerAction},
    Controller,
};
use log::{error, info};
use thiserror::Error;

#[derive(Error, Debug)]
enum ReconcileError {
    #[error("missing object spec")]
    MissingSpec,
}

async fn reconcile(s: Service, _ctx: Context<()>) -> Result<ReconcilerAction, ReconcileError> {
    info!(
        "Found a service {:?} in namespace {:?}",
        s.metadata.name, s.metadata.namespace,
    );

    let requeue_after = match s
        .spec
        .ok_or(ReconcileError::MissingSpec)?
        .type_
        .ok_or(ReconcileError::MissingSpec)?
        .as_str()
    {
        "LoadBalancer" => Some(Duration::from_secs(60)),
        _ => None,
    };

    Ok(ReconcilerAction { requeue_after })
}

fn error_policy(_error: &ReconcileError, _ctx: Context<()>) -> ReconcilerAction {
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(60)),
    }
}

#[actix_rt::main]
async fn main() -> Result<()> {
    env_logger::init();
    let client = Client::try_default().await?;
    let context = Context::new(());

    let services = Api::<Service>::all(client);

    Controller::new(services, ListParams::default())
        .run(reconcile, error_policy, context)
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(e) => error!("reconcile failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}
