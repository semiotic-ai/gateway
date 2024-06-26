use std::{collections::HashMap, time::Duration};

use alloy_primitives::Address;
use anyhow::anyhow;
use gateway_common::blocklist::Blocklist as _;
use itertools::Itertools;
use semver::Version;
use thegraph_core::types::SubgraphId;
use tokio::sync::Mutex;
use tracing::Instrument;
use url::Url;
use vec1::Vec1;

use self::types::{
    AllocationInfo, DeploymentInfo, IndexerIndexingProgressInfo, IndexerInfo, SubgraphInfo,
    SubgraphVersionInfo,
};
use super::{
    indexer_addr_blocklist::AddrBlocklist, indexer_host_blocklist::HostBlocklist,
    indexer_host_resolver::HostResolver, indexer_indexing_cost_model_compiler::CostModelCompiler,
    indexer_indexing_cost_model_resolver::CostModelResolver,
    indexer_indexing_poi_blocklist::PoiBlocklist, indexer_indexing_poi_resolver::PoiResolver,
    indexer_indexing_progress_resolver::IndexingProgressResolver,
    indexer_version_resolver::VersionResolver, snapshot, snapshot::NetworkTopologySnapshot,
    subgraph, subgraph::Client as SubgraphClient,
};

/// The network topology fetch timeout.
///
/// This timeout is applied independently to the indexers and subgraphs information fetches.
const NETWORK_TOPOLOGY_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Internal types.
pub mod types {
    use std::{collections::HashMap, fmt::Display};

    use alloy_primitives::{Address, BlockNumber};
    use cost_model::CostModel;
    use custom_debug::CustomDebug;
    use eventuals::Ptr;
    use semver::Version;
    use thegraph_core::types::{DeploymentId, SubgraphId};
    use url::Url;
    use vec1::Vec1;

    /// Internal representation of the fetched subgraph information.
    ///
    /// This is not the final representation of the subgraph.
    #[derive(Clone, Debug)]
    pub struct SubgraphInfo {
        pub id: SubgraphId,
        pub id_on_l2: Option<SubgraphId>,
        pub versions: Vec1<SubgraphVersionInfo>,
    }

    #[derive(Clone, Debug)]
    pub struct SubgraphVersionInfo {
        pub version: u32,
        pub deployment: DeploymentInfo,
    }

    /// Internal representation of the fetched deployment information.
    ///
    /// This is not the final representation of the deployment.
    #[derive(Clone, Debug)]
    pub struct DeploymentInfo {
        pub id: DeploymentId,
        pub allocations: Vec<AllocationInfo>,
        pub manifest_network: Option<String>,
        pub manifest_start_block: Option<BlockNumber>,
        pub transferred_to_l2: bool,
    }

    /// Internal representation of the fetched allocation information.
    ///
    /// This is not the final representation of the allocation.
    #[derive(Clone, Debug)]
    pub struct AllocationInfo {
        // The allocation ID.
        pub id: Address,
        // The indexer ID.
        pub indexer: Address,
    }

    /// Internal representation of the fetched indexer information.
    ///
    /// This is not the final representation of the indexer.
    #[derive(Clone, CustomDebug)]
    pub struct IndexerInfo {
        /// The indexer's ID.
        pub id: Address,
        /// The indexer's URL.
        ///
        /// It is guaranteed that the URL scheme is either HTTP or HTTPS and the URL has a host.
        #[debug(with = Display::fmt)]
        pub url: Url,
        /// The total amount of tokens staked by the indexer.
        pub staked_tokens: u128,
        /// The list of deployments the indexer is associated with.
        ///
        /// The deployments are ordered from highest to lowest associated token allocation.
        //  See ref: d260724b-a445-4842-964e-fb95062c119d
        pub deployments: Vec1<DeploymentId>,

        /// The indexer's "indexer service" version.
        pub indexer_agent_version: Version,
        /// The indexer's "graph node" version.
        pub graph_node_version: Version,

        /// The largest allocation per indexing.
        pub largest_allocation: HashMap<DeploymentId, Address>,
        /// The total amount of tokens allocated by the indexer per indexing.
        pub total_allocated_tokens: HashMap<DeploymentId, u128>,

        /// The indexer's indexing progress information.
        pub indexings_progress: HashMap<DeploymentId, IndexerIndexingProgressInfo>,
        /// The indexer's indexings cost models.
        pub indexings_cost_model: HashMap<DeploymentId, Ptr<CostModel>>,
    }

    /// Internal representation of the fetched indexer's indexing progress information.
    #[derive(Clone, Debug)]
    pub struct IndexerIndexingProgressInfo {
        /// The latest block the indexer has indexed for the deployment.
        pub latest_block: BlockNumber,
        /// The minimum block the indexer has indexed for the deployment.
        pub min_block: Option<BlockNumber>,
    }
}

/// Internal type holding the network service state.
pub struct InternalState {
    pub indexer_http_client: reqwest::Client,
    pub indexer_min_agent_version: Version,
    pub indexer_min_graph_node_version: Version,
    pub indexer_addr_blocklist: Option<AddrBlocklist>,
    pub indexer_host_resolver: Mutex<HostResolver>,
    pub indexer_host_blocklist: Option<HostBlocklist>,
    pub indexer_version_resolver: VersionResolver,
    pub indexer_indexing_pois_blocklist: Option<(PoiBlocklist, Mutex<PoiResolver>)>,
    pub indexer_indexing_status_resolver: IndexingProgressResolver,
    pub indexer_indexing_cost_model_resolver: (CostModelResolver, Mutex<CostModelCompiler>),
}

/// Fetch the network topology information from the graph network subgraph.
pub async fn fetch_update(
    client: &Mutex<SubgraphClient>,
    state: &InternalState,
) -> anyhow::Result<NetworkTopologySnapshot> {
    // Fetch and pre-process the network topology information
    let (indexers_info, subgraphs_info) = futures::future::try_join(
        async {
            let indexers = {
                let mut subgraph_client = client.lock().await;
                match tokio::time::timeout(
                    NETWORK_TOPOLOGY_FETCH_TIMEOUT,
                    fetch_and_pre_process_indexers_info(&mut subgraph_client),
                )
                .await
                {
                    // If the fetch timed out, return an error
                    Err(_) => Err(anyhow!("indexers info fetch timed out")),
                    Ok(resp) => match resp {
                        // If the fetch failed, return an error
                        Err(err) => Err(anyhow!("indexers info fetch failed: {err}")),
                        Ok(resp) => Ok(resp),
                    },
                }
            }?;

            // Process the fetched network topology information
            process_indexers_info(state, indexers).await
        },
        async {
            let mut subgraph_client = client.lock().await;
            match tokio::time::timeout(
                NETWORK_TOPOLOGY_FETCH_TIMEOUT,
                fetch_and_pre_process_subgraphs_info(&mut subgraph_client),
            )
            .await
            {
                // If the fetch timed out, return an error
                Err(_) => Err(anyhow!("subgraphs info fetch timed out")),
                Ok(resp) => match resp {
                    // If the fetch failed, return an error
                    Err(err) => Err(anyhow!("subgraphs info fetch failed: {err}")),
                    Ok(resp) => Ok(resp),
                },
            }
        },
    )
    .await?;

    Ok(snapshot::new_from(indexers_info, subgraphs_info))
}

/// Fetch the indexers information from the graph network subgraph and performs pre-processing
/// steps, i.e., validation and conversion into the internal representation.
///
///   1. Fetch the indexers information from the graph network subgraph.
///   2. Validate and convert the indexers fetched info into the internal representation.
///
/// If the fetch fails or the response is empty, an error is returned.
///
/// Invalid info is filtered out before converting into the internal representation. If no valid
/// indexers are found, an error is returned.
pub async fn fetch_and_pre_process_indexers_info(
    client: &mut SubgraphClient,
) -> anyhow::Result<HashMap<Address, IndexerInfo>> {
    // Fetch the indexers information from the graph network subgraph
    let indexers = client
        .fetch_indexers()
        .await
        .map_err(|err| anyhow!("indexers fetch failed: {err}"))?;
    if indexers.is_empty() {
        return Err(anyhow!("empty indexers fetch"));
    }

    // Map the fetched indexers info into the internal representation
    // If no valid indexers are found, an error is returned.
    let indexers = indexers
        .into_iter()
        .filter_map(|indexer| {
            let _span =
                tracing::debug_span!("indexer pre-processing", indexer.id = %indexer.id).entered();
            tracing::trace!(
                indexer.allocations_count = %indexer.allocations.len(),
                indexer.url = ?indexer.url,
            );

            match try_into_internal_indexer_info(indexer) {
                Ok(indexer) => Some((indexer.id, indexer)),
                Err(err) => {
                    tracing::debug!("filtering-out indexer: {err}");
                    None
                }
            }
        })
        .collect::<HashMap<_, _>>();

    // If no valid indexers are found, return an error
    if indexers.is_empty() {
        Err(anyhow!("no valid indexers found"))
    } else {
        Ok(indexers)
    }
}

/// Fetch the subgraphs information from the graph network subgraph and performs pre-processing
/// steps, i.e., validation and conversion into the internal representation.
///
///   1. Fetch the subgraphs information from the graph network subgraph.
///   2. Validate and convert the subgraphs fetched info into the internal representation.
///
/// If the fetch fails or the response is empty, an error is returned.
///
/// Invalid info is filtered out before converting into the internal representation. If no valid
/// subgraphs are found, an error is returned.
pub async fn fetch_and_pre_process_subgraphs_info(
    client: &mut SubgraphClient,
) -> anyhow::Result<HashMap<SubgraphId, SubgraphInfo>> {
    // Fetch the subgraphs information from the graph network subgraph
    let subgraphs = client
        .fetch_subgraphs()
        .await
        .map_err(|err| anyhow!("subgraphs fetch failed: {err}"))?;
    if subgraphs.is_empty() {
        return Err(anyhow!("empty subgraphs fetch"));
    }

    // Map the fetched subgraphs info into the internal representation
    // If no valid subgraphs are found, an error is returned.
    let subgraphs = subgraphs
        .into_iter()
        .filter_map(|subgraph| {
            let _span = tracing::debug_span!(
                "subgraph pre-processing",
                subgraph.id = %subgraph.id,
            )
            .entered();
            match try_into_internal_subgraph_info(subgraph) {
                Ok(subgraph) => Some((subgraph.id, subgraph)),
                Err(err) => {
                    tracing::debug!("filtering-out subgraph: {err}");
                    None
                }
            }
        })
        .collect::<HashMap<_, _>>();

    if subgraphs.is_empty() {
        Err(anyhow!("no valid subgraphs found"))
    } else {
        Ok(subgraphs)
    }
}

/// Convert from the fetched indexer information into the internal representation.
///
/// If the indexer is invalid, e.g., has no URL, an error is returned.
fn try_into_internal_indexer_info(
    indexer: subgraph::types::fetch_indexers::Indexer,
) -> anyhow::Result<IndexerInfo> {
    // Check if the indexer is present
    let indexer_url = indexer.url.ok_or_else(|| anyhow!("missing URL"))?;

    // Parse the URL. It must have an HTTP (or HTTPS) scheme and a valid host.
    // Filter out indexers with invalid URLs.
    let indexer_url = indexer_url
        .parse::<Url>()
        .map_err(|err| anyhow!("URL parsing failed: {err}"))?;
    if !indexer_url.scheme().starts_with("http") {
        return Err(anyhow!("invalid URL: invalid scheme"));
    }
    if indexer_url.host().is_none() {
        return Err(anyhow!("invalid URL: missing host"));
    }

    // Check if the indexer has any allocations
    let indexer_allocations: Vec1<_> = indexer
        .allocations
        .try_into()
        .map_err(|_| anyhow!("no allocations"))?;

    // Get the list of unique deployment IDs the indexer is associated with.
    // NOTE: The indexer is guaranteed to have at least one allocation and one
    // deployment.
    // See ref: d260724b-a445-4842-964e-fb95062c119d
    let indexer_deployment_ids: Vec1<_> = indexer_allocations
        .iter()
        .map(|alloc| alloc.subgraph_deployment.id)
        .unique()
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no deployments"))?;

    // Get the largest allocation and the total amount of tokens allocated for each indexing
    // NOTE: The allocations are ordered by `allocatedTokens` in descending order, and
    // the largest allocation is the first one.
    // See ref: d260724b-a445-4842-964e-fb95062c119d
    let indexer_indexing_largest_allocations = indexer_deployment_ids
        .iter()
        .flat_map(|deployment_id| {
            indexer_allocations
                .iter()
                .filter_map(|alloc| {
                    if alloc.subgraph_deployment.id == *deployment_id {
                        Some((*deployment_id, alloc.id))
                    } else {
                        None
                    }
                })
                .next()
        })
        .collect::<HashMap<_, _>>();

    let indexer_indexing_total_allocated_tokens = indexer_deployment_ids
        .iter()
        .map(|deployment_id| {
            let total = indexer_allocations
                .iter()
                .filter_map(|alloc| {
                    if alloc.subgraph_deployment.id == *deployment_id {
                        Some(alloc.allocated_tokens)
                    } else {
                        None
                    }
                })
                .sum();
            (*deployment_id, total)
        })
        .collect::<HashMap<_, _>>();

    Ok(IndexerInfo {
        id: indexer.id,
        url: indexer_url,
        staked_tokens: indexer.staked_tokens,
        deployments: indexer_deployment_ids,
        largest_allocation: indexer_indexing_largest_allocations,
        total_allocated_tokens: indexer_indexing_total_allocated_tokens,
        indexer_agent_version: Version::new(0, 0, 0), // Placeholder
        graph_node_version: Version::new(0, 0, 0),    // Placeholder
        indexings_progress: HashMap::new(),           // Placeholder
        indexings_cost_model: HashMap::new(),         // Placeholder
    })
}

/// Convert from the fetched subgraph information into the internal representation.
///
/// If the subgraph is invalid, e.g., has no versions, an error is returned.
fn try_into_internal_subgraph_info(
    subgraph: subgraph::types::fetch_subgraphs::Subgraph,
) -> anyhow::Result<SubgraphInfo> {
    let versions = subgraph
        .versions
        .into_iter()
        .map(|version| {
            let deployment = version.subgraph_deployment;

            let deployment_manifest_network = deployment
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.network.clone());
            let deployment_manifest_start_block = deployment
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.start_block);

            let deployment_allocations = deployment
                .allocations
                .into_iter()
                .map(|allocation| AllocationInfo {
                    id: allocation.id,
                    indexer: allocation.indexer.id,
                })
                .collect::<Vec<_>>();

            let deployment_id = deployment.id;
            let deployment_transferred_to_l2 = deployment.transferred_to_l2;

            let version_number = version.version;
            let version_deployment = DeploymentInfo {
                id: deployment_id,
                allocations: deployment_allocations,
                manifest_network: deployment_manifest_network,
                manifest_start_block: deployment_manifest_start_block,
                transferred_to_l2: deployment_transferred_to_l2,
            };

            SubgraphVersionInfo {
                version: version_number,
                deployment: version_deployment,
            }
        })
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no valid versions found"))?;

    Ok(SubgraphInfo {
        id: subgraph.id,
        id_on_l2: subgraph.id_on_l2,
        versions,
    })
}

/// Process the fetched network topology information.
pub async fn process_indexers_info(
    state: &InternalState,
    indexers: HashMap<Address, IndexerInfo>,
) -> anyhow::Result<HashMap<Address, IndexerInfo>> {
    // Process the fetched indexers information
    let indexers_info = {
        let indexers_iter_fut = indexers.into_iter().map(move |(indexer_id, indexer)| {
            // Instrument the indexer processing span
            let indexer_span = tracing::debug_span!(
                "indexer processing",
                indexer.id = %indexer.id,
                indexer.url = %indexer.url,
                indexer.agent_version = tracing::field::Empty,
                indexer.graph_node_version = tracing::field::Empty,
            );
            tracing::trace!(parent: &indexer_span, "processing");

            async move {
                let mut indexer = indexer;

                // Check if the indexer's address is in the address blocklist
                if let Err(err) =
                    check_indexer_blocked_by_addr_blocklist(&state.indexer_addr_blocklist, &indexer)
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Check if the indexer's host is in the host blocklist
                if let Err(err) = resolve_and_check_indexer_blocked_by_host_blocklist(
                    &state.indexer_host_resolver,
                    &state.indexer_host_blocklist,
                    &indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Check if the indexer's reported versions are supported
                if let Err(err) = resolve_and_check_indexer_blocked_by_version(
                    &state.indexer_version_resolver,
                    &state.indexer_min_agent_version,
                    &state.indexer_min_graph_node_version,
                    &mut indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Update the span information with the resolved versions
                tracing::Span::current()
                    .record(
                        "indexer.agent_version",
                        tracing::field::display(&indexer.indexer_agent_version),
                    )
                    .record(
                        "indexer.graph_node_version",
                        tracing::field::display(&indexer.graph_node_version),
                    );

                // Check if the indexer's deployments should be blocked by POI
                // Update the indexer's deployments list to only include the deployments that are
                // not blocked by POI. If the indexer has no deployments left, it must be ignored.
                if let Err(err) = resolve_and_check_indexer_blocked_by_poi(
                    &state.indexer_indexing_pois_blocklist,
                    &mut indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Fetch the indexer's indexing progress statuses
                // NOTE: At this point, the indexer's deployments list should contain only the
                //       deployment IDs that were not blocked by any blocklist.
                if let Err(err) = resolve_indexer_indexing_progress_statuses(
                    &state.indexer_indexing_status_resolver,
                    &mut indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Fetch the indexer's indexing statuses and cost models
                // NOTE: At this point, the indexer's deployments list should contain only the
                //       deployment IDs that were not blocked by any blocklist.
                if let Err(err) = resolve_indexer_indexing_cost_models(
                    &state.indexer_indexing_cost_model_resolver,
                    &mut indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                Some((indexer_id, indexer))
            }
            .instrument(indexer_span)
        });

        // Wait for all the indexers to be processed
        futures::future::join_all(indexers_iter_fut).await
    }
    .into_iter()
    .flatten() // Filter out the `None` values
    .collect::<HashMap<_, _>>();

    if indexers_info.is_empty() {
        Err(anyhow!("no valid indexers found"))
    } else {
        Ok(indexers_info)
    }
}

/// Check if the indexer's address is in the address blocklist.
///
/// - If the address blocklist was not configured: the indexer is ALLOWED.
/// - If the address is in the blocklist: the indexer is BLOCKED.
fn check_indexer_blocked_by_addr_blocklist(
    blocklist: &Option<AddrBlocklist>,
    indexer: &IndexerInfo,
) -> anyhow::Result<()> {
    let blocklist = match blocklist {
        Some(blocklist) => blocklist,
        None => return Ok(()),
    };

    // Check if the indexer's address is in the blocklist
    if blocklist.check(&indexer.id).is_blocked() {
        return Err(anyhow!("indexer address blocked by blocklist"));
    }

    Ok(())
}

/// Resolve and check if the indexer's host is in the host blocklist.
///
/// - If the indexer's host is not resolvable: the indexer is BLOCKED.
/// - If the host blocklist was not configured: the indexer is ALLOWED.
/// - If the indexer's host is in the blocklist: the indexer is BLOCKED.
async fn resolve_and_check_indexer_blocked_by_host_blocklist(
    resolver: &Mutex<HostResolver>,
    blocklist: &Option<HostBlocklist>,
    indexer: &IndexerInfo,
) -> anyhow::Result<()> {
    // Resolve the indexer's URL, if it fails (or times out), the indexer must be BLOCKED
    let mut host_resolver = resolver.lock().await;
    let resolution_result = match host_resolver.resolve_url(&indexer.url).await {
        // If the resolution failed, the indexer must be BLOCKED
        Err(err) => {
            return Err(anyhow!("failed to resolve indexer URL: {err}"));
        }
        Ok(result) => result,
    };

    // If the host blocklist was not configured, the indexer must be ALLOWED
    let host_blocklist = match blocklist {
        Some(blocklist) => blocklist,
        _ => return Ok(()),
    };

    if host_blocklist.check(&resolution_result).is_blocked() {
        return Err(anyhow!("indexer host blocked by blocklist"));
    }

    Ok(())
}

/// Resolve and check if the indexer's reported versions are supported.
///
/// - If the agent version is not resolvable: the indexer must be BLOCKED.
/// - If the agent version is below the minimum required: the indexer must be BLOCKED.
/// - If the graph node version is not resolvable: the indexer must be BLOCKED.
/// - If the graph node version is below the minimum required: the indexer must be BLOCKED.
async fn resolve_and_check_indexer_blocked_by_version(
    resolver: &VersionResolver,
    min_agent_version: &Version,
    min_graph_node_version: &Version,
    indexer: &mut IndexerInfo,
) -> anyhow::Result<()> {
    // Resolve the indexer's agent version
    let agent_version = match resolver.resolve_agent_version(&indexer.url).await {
        // If the resolution failed, the indexer must be BLOCKED
        Err(err) => {
            return Err(anyhow!("agent version resolution failed: {err}"));
        }
        Ok(result) => result,
    };

    // Check if the indexer's agent version is supported
    if agent_version < *min_agent_version {
        return Err(anyhow!(
            "agent version {} below the minimum required {}",
            agent_version,
            min_agent_version
        ));
    }

    // Resolve the indexer's graph node version, with a timeout
    let graph_node_version = match resolver.resolve_graph_node_version(&indexer.url).await {
        // If the resolution failed, the indexer must be BLOCKED
        Err(err) => {
            // TODO: After more graph nodes support reporting their version,
            //  we should assume they are on the minimum version if we can't
            //  get the version.
            tracing::trace!("graph-node version resolution failed: {err}");
            min_graph_node_version.clone()
        }
        Ok(result) => result,
    };

    // Check if the indexer's graph node version is supported
    if graph_node_version < *min_graph_node_version {
        return Err(anyhow!(
            "Graph node version {} below the minimum required {}",
            graph_node_version,
            min_graph_node_version
        ));
    }

    // Set the indexer's versions
    indexer.indexer_agent_version = agent_version;
    indexer.graph_node_version = graph_node_version;

    Ok(())
}

/// Resolve and check if any of the indexer's deployments should be blocked by POI.
///
/// - If the POI blocklist was not configured: the indexer must be ALLOWED.
/// - If not indexing any of the affected deployments: the indexer must be ALLOWED.
/// - If there are no healthy indexings, i.e., all indexings are blocked: the indexer must be BLOCKED.
async fn resolve_and_check_indexer_blocked_by_poi(
    blocklist: &Option<(PoiBlocklist, Mutex<PoiResolver>)>,
    indexer: &mut IndexerInfo,
) -> anyhow::Result<()> {
    // If the POI blocklist was not configured, the indexer must be ALLOWED
    let (pois_blocklist, pois_resolver) = match blocklist {
        Some((blocklist, resolver)) => (blocklist, resolver),
        _ => return Ok(()),
    };

    // Get the list of affected POIs to resolve for the indexer's deployments
    // If none of the deployments are affected, the indexer must be ALLOWED
    let indexer_affected_pois = pois_blocklist.affected_pois_metadata(&indexer.deployments);
    if indexer_affected_pois.is_empty() {
        return Ok(());
    }

    // Resolve the indexer public POIs for the affected deployments
    let poi_result = {
        let mut pois_resolver = pois_resolver.lock().await;
        pois_resolver
            .resolve(&indexer.url, &indexer_affected_pois)
            .await?
    };

    // Check if any of the reported POIs are in the blocklist. and filter out the indexings
    // Update the indexers deployments to only include the deployments that are not affected
    // i.e., keep the deployments that are not blocked by POI.
    let check_result = pois_blocklist.check(poi_result);
    indexer
        .deployments
        .retain(|id| match check_result.get(id) {
            Some(state) => state.is_allowed(),
            None => {
                // If the deployment is not affected, keep it
                true
            }
        })
        // If all deployments are blocked, the indexer must be BLOCKED
        .map_err(|_| anyhow!("all deployments blocked due to blocked POIs"))?;

    Ok(())
}

/// Resolve the indexer's indexing progress status.
async fn resolve_indexer_indexing_progress_statuses(
    resolver: &IndexingProgressResolver,
    indexer: &mut IndexerInfo,
) -> anyhow::Result<()> {
    let progress_status = match resolver.resolve(&indexer.url, &indexer.deployments).await {
        // If the resolution failed, the indexer must be BLOCKED
        Err(err) => {
            return Err(anyhow!("indexing progress status resolution failed: {err}"));
        }
        Ok(result) => result,
    };
    tracing::trace!(
        indexings = %indexer.deployments.len(),
        indexing_status = %progress_status.len(),
        "indexing progress status resolved"
    );

    let indexing_progress = progress_status
        .into_iter()
        .map(|(deployment_id, res)| {
            (
                deployment_id,
                IndexerIndexingProgressInfo {
                    latest_block: res.latest_block,
                    min_block: res.min_block,
                },
            )
        })
        .collect();

    // Set the indexer's indexing progress status
    indexer.indexings_progress = indexing_progress;

    Ok(())
}

/// Resolve the indexer's indexing cost models.
async fn resolve_indexer_indexing_cost_models(
    (resolver, compiler): &(CostModelResolver, Mutex<CostModelCompiler>),
    indexer: &mut IndexerInfo,
) -> anyhow::Result<()> {
    // Resolve the indexer's cost model sources
    let indexings_cost_models = match resolver.resolve(&indexer.url, &indexer.deployments).await {
        Err(err) => {
            // If the resolution failed, return early
            tracing::debug!("cost model resolution failed: {err}");
            return Ok(());
        }
        Ok(result) if result.is_empty() => {
            // If the resolution is empty, return early
            return Ok(());
        }
        Ok(result) => result,
    };

    // Compile the cost model sources into cost models
    let indexings_cost_models = {
        let mut compiler = compiler.lock().await;
        indexings_cost_models
            .into_iter()
            .filter_map(|(deployment, source)| match compiler.compile(source) {
                Err(err) => {
                    tracing::debug!("cost model compilation failed: {err}");
                    None
                }
                Ok(cost_model) => Some((deployment, cost_model)),
            })
            .collect()
    };

    // Set the indexer's indexing cost models
    indexer.indexings_cost_model = indexings_cost_models;

    Ok(())
}
