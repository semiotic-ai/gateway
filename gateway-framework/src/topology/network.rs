use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use alloy_primitives::Address;
use eventuals::{Eventual, EventualExt, Ptr};
use futures::future::join_all;
use gateway_common::types::Indexing;
use itertools::Itertools;
use thegraph_core::types::{DeploymentId, SubgraphId};
use tokio::sync::Mutex;
use url::Url;

use crate::{ip_blocker::IpBlocker, network::network_subgraph};

/// Deployment manifest information needed for the gateway to work.
pub struct Manifest {
    pub network: String,
    pub min_block: u64,
}

pub struct Indexer {
    pub id: Address,
    pub url: Url,
    pub staked_tokens: u128,
    pub largest_allocation: Address,
    pub allocated_tokens: u128,
}

impl Indexer {
    pub fn cost_url(&self) -> Url {
        // Indexer URLs are validated when they are added to the network, so this should never fail.
        // 7f2f89aa-24c9-460b-ab1e-fc94697c4f4
        self.url.join("cost").unwrap()
    }

    pub fn status_url(&self) -> Url {
        // Indexer URLs are validated when they are added to the network, so this should never fail.
        // 7f2f89aa-24c9-460b-ab1e-fc94697c4f4
        self.url.join("status").unwrap()
    }
}

/// In an effort to keep the ownership structure a simple tree, this only contains the info required
/// to resolve queries by `SubgraphId` into the relevant deployments. Therefore, there is no need
/// for a query by `DeploymentId` to interact with this.
#[derive(Clone)]
pub struct Subgraph {
    /// Subgraph versions, in ascending order
    pub deployments: Vec<Arc<Deployment>>,
    pub id: SubgraphId,
    /// Indicates that the subgraph has been transferred to L2, and should not be served directly by
    /// this gateway.
    pub l2_id: Option<SubgraphId>,
}

pub struct Deployment {
    pub id: DeploymentId,
    pub manifest: Manifest,
    /// An indexer may have multiple active allocations on a deployment. We collapse them into a
    /// single logical allocation using the largest allocation ID and sum of the allocated tokens.
    pub indexers: HashMap<Address, Arc<Indexer>>,
    /// A deployment may be associated with multiple subgraphs.
    pub subgraphs: BTreeSet<SubgraphId>,
    /// Indicates that the deployment should not be served directly by this gateway. This will
    /// always be false when `allocations > 0`.
    pub transferred_to_l2: bool,
}

pub struct Allocation {
    pub id: Address,
    pub allocated_tokens: u128,
    pub indexer: Arc<Indexer>,
}

/// Representation of the graph network being used to serve queries
#[derive(Clone)]
pub struct GraphNetwork {
    pub subgraphs: Eventual<Ptr<HashMap<SubgraphId, Subgraph>>>,
    pub deployments: Eventual<Ptr<HashMap<DeploymentId, Arc<Deployment>>>>,
    pub indexers: Eventual<Ptr<HashMap<Address, Arc<Indexer>>>>,
}

impl GraphNetwork {
    pub async fn new(
        subgraphs: Eventual<Ptr<Vec<network_subgraph::Subgraph>>>,
        ip_blocker: IpBlocker,
    ) -> Self {
        let ip_blocker: &'static Mutex<IpBlocker> = Box::leak(Box::new(ip_blocker.into()));

        // Create a lookup table for subgraphs, keyed by their ID.
        // Invalid URL indexers are filtered out. See ref: 7f2f89aa-24c9-460b-ab1e-fc94697c4f4
        let subgraphs = subgraphs.map(move |subgraphs| async move {
            Ptr::new(Self::subgraphs(&subgraphs, ip_blocker).await)
        });

        // Create a lookup table for deployments, keyed by their ID (which is also their IPFS hash).
        let deployments = subgraphs.clone().map(|subgraphs| async move {
            subgraphs
                .values()
                .flat_map(|subgraph| &subgraph.deployments)
                .map(|deployment| (deployment.id, deployment.clone()))
                .collect::<HashMap<DeploymentId, Arc<Deployment>>>()
                .into()
        });

        // Create a lookup table for indexers, keyed by their ID (which is also their address).
        let indexers = subgraphs.clone().map(|subgraphs| async move {
            subgraphs
                .values()
                .flat_map(|subgraph| &subgraph.deployments)
                .flat_map(|deployment| &deployment.indexers)
                .map(|(id, indexer)| (*id, indexer.clone()))
                .collect::<HashMap<Address, Arc<Indexer>>>()
                .into()
        });

        // Return only after eventuals have values, to avoid serving client queries prematurely.
        if deployments.value().await.is_err() || indexers.value().await.is_err() {
            panic!("Failed to await Graph network topology");
        }

        Self {
            subgraphs,
            deployments,
            indexers,
        }
    }

    async fn subgraphs(
        subgraphs: &[network_subgraph::Subgraph],
        ip_blocker: &'static Mutex<IpBlocker>,
    ) -> HashMap<SubgraphId, Subgraph> {
        join_all(subgraphs.iter().map(|subgraph| async move {
            let id = subgraph.id;
            let deployments = join_all(
                subgraph
                    .versions
                    .iter()
                    .map(|version| Self::deployment(subgraphs, version, ip_blocker)),
            )
            .await
            .into_iter()
            .flatten()
            .collect();
            let subgraph = Subgraph {
                deployments,
                id,
                l2_id: subgraph.id_on_l2,
            };
            (id, subgraph)
        }))
        .await
        .into_iter()
        .collect()
    }

    async fn deployment(
        subgraphs: &[network_subgraph::Subgraph],
        version: &network_subgraph::SubgraphVersion,
        ip_blocker: &'static Mutex<IpBlocker>,
    ) -> Option<Arc<Deployment>> {
        let id = version.subgraph_deployment.id;
        let manifest = version.subgraph_deployment.manifest.as_ref()?;
        let manifest = Manifest {
            network: manifest.network.as_ref()?.clone(),
            min_block: manifest.start_block.unwrap_or(0),
        };
        let subgraphs = subgraphs
            .iter()
            .filter(|subgraph| {
                subgraph
                    .versions
                    .iter()
                    .any(|v| v.subgraph_deployment.id == id)
            })
            .map(|subgraph| subgraph.id)
            .collect();

        // extract indexer info from each allocation
        let mut indexers: HashMap<Address, Arc<Indexer>> = version
            .subgraph_deployment
            .allocations
            .iter()
            .filter_map(|allocation| {
                // If indexer URL parsing fails, the allocation is ignored (filtered out).
                // 7f2f89aa-24c9-460b-ab1e-fc94697c4f4
                let url = allocation.indexer.url.as_ref()?.parse().ok()?;

                let id = allocation.indexer.id;
                Some((
                    id,
                    Indexer {
                        id,
                        url,
                        staked_tokens: allocation.indexer.staked_tokens,
                        largest_allocation: allocation.id,
                        allocated_tokens: allocation.allocated_tokens,
                    },
                ))
            })
            .into_group_map() // TODO: remove need for itertools here: https://github.com/rust-lang/rust/issues/80552
            .into_iter()
            .filter_map(|(_, mut allocations)| {
                let total_allocation = allocations.iter().map(|a| a.allocated_tokens).sum();
                // last allocation is latest: 9936786a-e286-45f3-9190-8409d8389e88
                let mut indexer = allocations.pop()?;
                indexer.allocated_tokens = total_allocation;
                Some(indexer)
            })
            .map(|indexer| (indexer.id, indexer.into()))
            .collect();

        let mut blocked: BTreeSet<Address> = Default::default();
        {
            let mut ip_blocker = ip_blocker.lock().await;
            for indexer in indexers.values() {
                if let Err(ip_block) = ip_blocker.is_ip_blocked(&indexer.url).await {
                    tracing::info!(ip_block, indexer = ?indexer.id, url = %indexer.url);
                    blocked.insert(indexer.id);
                }
            }
        }
        indexers.retain(|indexer, _| !blocked.contains(indexer));

        // abf62a6d-c071-4507-b528-ddc8e250127a
        let transferred_to_l2 = version.subgraph_deployment.transferred_to_l2
            && version.subgraph_deployment.allocations.is_empty();

        Some(Arc::new(Deployment {
            id,
            manifest,
            subgraphs,
            indexers,
            transferred_to_l2,
        }))
    }

    /// Get the subgraph by ID ([SubgraphId]), if it exists.
    pub fn subgraph_by_id(&self, id: &SubgraphId) -> Option<Subgraph> {
        self.subgraphs.value_immediate()?.get(id).cloned()
    }

    /// Get the deployment by ID ([DeploymentId]), if it exists.
    pub fn deployment_by_id(&self, id: &DeploymentId) -> Option<Arc<Deployment>> {
        self.deployments.value_immediate()?.get(id).cloned()
    }

    // Get then indexer data for some deployment.
    pub fn indexing(&self, indexing: &Indexing) -> Option<Arc<Indexer>> {
        self.deployments
            .value_immediate()?
            .get(&indexing.deployment)?
            .indexers
            .get(&indexing.indexer)
            .cloned()
    }
}
