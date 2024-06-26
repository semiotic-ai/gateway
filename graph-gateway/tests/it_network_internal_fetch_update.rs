use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
use anyhow::anyhow;
use assert_matches::assert_matches;
use graph_gateway::network::{
    indexer_addr_blocklist::AddrBlocklist,
    indexer_host_blocklist::HostBlocklist,
    indexer_host_resolver::HostResolver,
    indexer_indexing_cost_model_compiler::CostModelCompiler,
    indexer_indexing_cost_model_resolver::CostModelResolver,
    indexer_indexing_progress_resolver::IndexingProgressResolver,
    indexer_version_resolver::{VersionResolver, DEFAULT_INDEXER_VERSION_RESOLUTION_TIMEOUT},
    internal::{
        fetch_and_pre_process_indexers_info as internal_fetch_and_pre_process_indexers_info,
        fetch_update as internal_fetch_update, process_indexers_info, types as internal_types,
        InternalState,
    },
    subgraph::Client,
    NetworkTopologySnapshot,
};
use ipnetwork::IpNetwork;
use semver::Version;
use thegraph_core::client::Client as SubgraphClient;
use tokio::sync::{Mutex, OnceCell};
use tracing_subscriber::{fmt::TestWriter, EnvFilter};
use url::Url;

// Test method to initialize the tests tracing subscriber.
fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .compact()
        .with_writer(TestWriter::default())
        .try_init();
}

/// Test helper to get the test url from the environment.
fn test_base_url() -> Url {
    std::env::var("IT_TEST_ARBITRUM_GATEWAY_URL")
        .expect("Missing IT_TEST_ARBITRUM_GATEWAY_URL")
        .parse()
        .expect("Invalid IT_TEST_ARBITRUM_GATEWAY_URL")
}

/// Test helper to get the test auth token from the environment.
fn test_auth_token() -> String {
    std::env::var("IT_TEST_ARBITRUM_GATEWAY_AUTH").expect("Missing IT_TEST_ARBITRUM_GATEWAY_AUTH")
}

/// Test helper to build the subgraph url with the given subgraph ID.
fn url_with_subgraph_id(name: impl AsRef<str>) -> Url {
    test_base_url()
        .join(&format!("api/subgraphs/id/{}", name.as_ref()))
        .expect("Invalid URL")
}

/// The Graph Network Arbitrum in the network.
///
/// https://thegraph.com/explorer/subgraphs/DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp
const GRAPH_NETWORK_ARBITRUM_SUBGRAPH_ID: &str = "DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp";

/// Test helper to get an [`Address`] from a given string.
fn test_address(addr: impl AsRef<str>) -> Address {
    addr.as_ref().parse().expect("Invalid address")
}

/// Test helper to build the service config for the tests.
fn test_service_state(
    addr_blocklist: HashSet<Address>,
    host_blocklist: HashSet<IpNetwork>,
    min_versions: Option<(Version, Version)>,
) -> Arc<InternalState> {
    let indexers_http_client = reqwest::Client::new();
    let indexers_host_resolver =
        Mutex::new(HostResolver::new().expect("Failed to create host resolver"));
    let indexers_version_resolver = VersionResolver::with_timeout(
        indexers_http_client.clone(),
        DEFAULT_INDEXER_VERSION_RESOLUTION_TIMEOUT, // 1500 ms
    );
    let indexers_indexing_status_resolver =
        IndexingProgressResolver::new(indexers_http_client.clone());
    let indexers_cost_model_resolver = (
        CostModelResolver::new(indexers_http_client.clone()),
        Mutex::new(CostModelCompiler::default()),
    );

    let mut state = InternalState {
        indexer_http_client: indexers_http_client.clone(),
        indexer_min_agent_version: Version::new(0, 0, 0),
        indexer_min_graph_node_version: Version::new(0, 0, 0),
        indexer_addr_blocklist: None,
        indexer_host_resolver: indexers_host_resolver,
        indexer_host_blocklist: None,
        indexer_version_resolver: indexers_version_resolver,
        indexer_indexing_pois_blocklist: None,
        indexer_indexing_status_resolver: indexers_indexing_status_resolver,
        indexer_indexing_cost_model_resolver: indexers_cost_model_resolver,
    };

    if !addr_blocklist.is_empty() {
        let indexers_addr_blocklist = AddrBlocklist::new(addr_blocklist);
        state.indexer_addr_blocklist = Some(indexers_addr_blocklist);
    }

    if !host_blocklist.is_empty() {
        let indexers_host_blocklist = HostBlocklist::new(host_blocklist);
        state.indexer_host_blocklist = Some(indexers_host_blocklist);
    }

    if let Some((min_agent_version, min_graph_node_version)) = min_versions {
        state.indexer_min_agent_version = min_agent_version;
        state.indexer_min_graph_node_version = min_graph_node_version;
    }

    Arc::new(state)
}

/// Test suite internal state to store the fetched network topology to avoid fetching it multiple
/// times during the tests.
static FETCHED_NETWORK_INFO: OnceCell<HashMap<Address, internal_types::IndexerInfo>> =
    OnceCell::const_new();

/// Test helper to fetch the network topology information.
///
/// The network topology information is fetched from the hosted service and pre-processed. The
/// result is cached to avoid fetching it multiple times during the tests.
///
/// This is a wrapper around the `service_internal::fetch_network_topology_info` method.
async fn fetch_and_pre_process_indexers_info() -> HashMap<Address, internal_types::IndexerInfo> {
    FETCHED_NETWORK_INFO
        .get_or_try_init(move || async move {
            let subgraph_url = url_with_subgraph_id(GRAPH_NETWORK_ARBITRUM_SUBGRAPH_ID);
            let auth_token = test_auth_token();

            let mut client = {
                let http_client = reqwest::Client::new();
                let subgraph_client = SubgraphClient::builder(http_client, subgraph_url)
                    .with_auth_token(Some(auth_token))
                    .build();
                Client::new(subgraph_client, true)
            };

            let indexers = internal_fetch_and_pre_process_indexers_info(&mut client)
                .await
                .map_err(|err| {
                    anyhow!("Failed to fetch and pre-process the indexers info: {err}")
                })?;

            Ok::<_, anyhow::Error>(indexers)
        })
        .await
        .cloned()
        .expect("Failed to fetch network topology")
}

/// Test helper to fetch, process and construct the network topology snapshot.
async fn fetch_update(service: &InternalState) -> anyhow::Result<NetworkTopologySnapshot> {
    let subgraph_url = url_with_subgraph_id(GRAPH_NETWORK_ARBITRUM_SUBGRAPH_ID);
    let auth_token = test_auth_token();

    let client = {
        let http_client = reqwest::Client::new();
        let subgraph_client = SubgraphClient::builder(http_client, subgraph_url)
            .with_auth_token(Some(auth_token))
            .build();
        Mutex::new(Client::new(subgraph_client, true))
    };

    internal_fetch_update(&client, service).await
}

#[test_with::env(IT_TEST_ARBITRUM_GATEWAY_URL, IT_TEST_ARBITRUM_GATEWAY_AUTH)]
#[tokio::test]
async fn fetch_a_network_topology_update() {
    init_test_tracing();

    //* Given
    let service = test_service_state(
        Default::default(), // No address blocklist
        Default::default(), // No host blocklist
        // Minimum versions, different from the default values to assert the versions are set.
        Some((
            Version::new(0, 0, 1), // Indexer agent version
            Version::new(0, 0, 1), // Graph node version
        )),
    );

    //* When
    let network = tokio::time::timeout(Duration::from_secs(30), fetch_update(&service))
        .await
        .expect("Topology fetch did not complete in time (30s)")
        .expect("Failed to fetch network topology");

    //* Then
    // Assert that the network topology is not empty.
    assert!(
        !network.subgraphs().is_empty(),
        "Network subgraphs are empty"
    );
    assert!(
        !network.deployments().is_empty(),
        "Network deployments are empty"
    );

    // Given a SUBGRAPH
    //- Assert that it has at least one indexing associated.
    assert!(
        network
            .subgraphs()
            .values()
            .all(|subgraph| !subgraph.indexings.is_empty()),
        "Subgraph has no indexings associated"
    );

    //- Assert the associated deployments' list is not empty.
    assert!(
        network
            .subgraphs()
            .values()
            .all(|subgraph| !subgraph.deployments.is_empty()),
        "Subgraph has no deployments associated"
    );

    //- Assert that all the indexings' deployments are contained in its deployments list.
    assert!(
        network.subgraphs().values().all(|subgraph| {
            subgraph.indexings.iter().all(|(indexing_id, indexing)| {
                subgraph.deployments.contains(&indexing_id.deployment)
                    && subgraph.deployments.contains(&indexing.id.deployment)
            })
        }),
        "Subgraph indexings deployments are not contained in the subgraph's deployments list"
    );

    //- Assert that all the associated indexings' indexers contain the indexing deployment ID in
    //  their indexings list.
    assert!(
        network.subgraphs().values().all(|subgraph| {
            subgraph.indexings.iter().all(|(indexing_id, indexing)| {
                indexing.indexer.indexings.contains(&indexing_id.deployment)
                    && indexing.indexer.indexings.contains(&indexing.id.deployment)
            })
        }),
        "Subgraph indexings deployment ID not found in the indexer's indexings list"
    );

    //- Assert that all the associated indexings' indexers versions are set.
    assert!(
        network.subgraphs().values().all(|subgraph| {
            subgraph.indexings.iter().all(|(_, indexing)| {
                indexing.indexer.indexer_agent_version >= Version::new(0, 0, 1)
                    && indexing.indexer.graph_node_version >= Version::new(0, 0, 1)
            })
        }),
        "Subgraph indexings indexer versions are not set"
    );

    //- Assert that some of the associated indexings' have reported a valid indexing status and
    //  cost model.
    assert!(
        network.subgraphs().values().any(|subgraph| {
            subgraph
                .indexings
                .values()
                .any(|indexing| indexing.status.is_some())
        }),
        "No subgraph indexings have a status"
    );
    assert!(
        network.subgraphs().values().any(|subgraph| {
            subgraph
                .indexings
                .values()
                .any(|indexing| indexing.cost_model.is_some())
        }),
        "No subgraph indexings have a cost model"
    );

    // Given a DEPLOYMENT
    //- Assert that it has at least one indexing associated.
    assert!(
        network
            .deployments()
            .values()
            .all(|deployment| !deployment.indexings.is_empty()),
        "Deployment has no indexings associated"
    );

    //- Assert that all the indexings' are correctly associated with the deployment.
    assert!(
        network.deployments().values().all(|deployment| {
            deployment.indexings.iter().all(|(indexing_id, indexing)| {
                indexing_id.deployment == deployment.id && indexing.id.deployment == deployment.id
            })
        }),
        "Incorrect indexing associated with the deployment"
    );

    //- Assert that all the associated indexings' indexers contain the indexing deployment ID in
    //  their indexings list.
    assert!(
        network.deployments().values().all(|deployment| {
            deployment.indexings.iter().all(|(indexing_id, indexing)| {
                indexing.indexer.indexings.contains(&indexing_id.deployment)
                    && indexing.indexer.indexings.contains(&indexing.id.deployment)
            })
        }),
        "Deployment indexings deployment ID not found in the indexer's indexings list"
    );

    //- Assert that all the associated indexings' indexers versions are set.
    assert!(
        network.subgraphs().values().all(|subgraph| {
            subgraph.indexings.iter().all(|(_, indexing)| {
                indexing.indexer.indexer_agent_version >= Version::new(0, 0, 1)
                    && indexing.indexer.graph_node_version >= Version::new(0, 0, 1)
            })
        }),
        "Subgraph indexings indexer versions are not set"
    );

    //- Assert that some of the associated indexings' have reported a valid indexing status and
    //  cost model.
    assert!(
        network.deployments().values().any(|deployment| {
            deployment
                .indexings
                .values()
                .any(|indexing| indexing.status.is_some())
        }),
        "No deployment indexings have a status"
    );
    assert!(
        network.deployments().values().any(|deployment| {
            deployment
                .indexings
                .values()
                .any(|indexing| indexing.cost_model.is_some())
        }),
        "No deployment indexings have a cost model"
    );

    // CROSS-CHECKS
    //- Assert that given a subgraph, all the associated deployments contain the subgraph ID in
    //  their subgraphs list.
    assert!(
        network.subgraphs().values().all(|subgraph| {
            subgraph.deployments.iter().all(|deployment_id| {
                network
                    .deployments()
                    .get(deployment_id)
                    .expect("Deployment not found")
                    .subgraphs
                    .contains(&subgraph.id)
            })
        }),
        "Subgraph associated deployment not found in the network deployments list"
    );

    //- Assert that given a deployment, all the associated subgraphs contain the deployment ID in
    //  their deployments list.
    assert!(
        network.deployments().values().all(|deployment| {
            deployment.subgraphs.iter().all(|subgraph_id| {
                network
                    .subgraphs()
                    .get(subgraph_id)
                    .expect("Subgraph not found")
                    .deployments
                    .contains(&deployment.id)
            })
        }),
        "Deployment associated subgraph not found in the network subgraphs list"
    );
}

#[test_with::env(IT_TEST_ARBITRUM_GATEWAY_URL, IT_TEST_ARBITRUM_GATEWAY_AUTH)]
#[tokio::test]
async fn fetch_indexers_info_and_block_an_indexer_by_address() {
    init_test_tracing();

    //* Given
    // The Indexer ID (address) of the 'https://indexer.upgrade.thegraph.com/' indexer
    let address = test_address("0xbdfb5ee5a2abf4fc7bb1bd1221067aef7f9de491");

    let addr_blocklist = HashSet::from([address]);
    let service = test_service_state(
        addr_blocklist,
        Default::default(), // No host blocklist
        Default::default(), // No minimum versions
    );

    // Fetch and pre-process the network topology information
    let indexers_info = tokio::time::timeout(
        Duration::from_secs(10),
        fetch_and_pre_process_indexers_info(),
    )
    .await
    .expect("Topology fetch did not complete in time (10s)");

    // Require the pre-processed info to contain the "test indexer"
    assert!(
        indexers_info.keys().any(|addr| *addr == address),
        "Test indexer not found in the indexers info"
    );

    //* When
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        process_indexers_info(&service, indexers_info),
    )
    .await
    .expect("Topology processing did not complete in time (20s)");

    //* Then
    let indexers_processed_info = res.expect("Failed to process indexers info");

    // Assert that the blocked indexer is not present in the indexers processed info
    assert!(
        indexers_processed_info.keys().all(|addr| *addr != address),
        "Blocked indexer is present in the indexers processed info"
    );
}

#[test_with::env(IT_TEST_ARBITRUM_GATEWAY_URL, IT_TEST_ARBITRUM_GATEWAY_AUTH)]
#[tokio::test]
async fn fetch_indexers_info_and_block_an_indexer_by_host() {
    init_test_tracing();

    //* Given
    // The Indexer ID (address) of the 'https://indexer.upgrade.thegraph.com/' indexer
    let address = test_address("0xbdfb5ee5a2abf4fc7bb1bd1221067aef7f9de491");

    // The IP network of the 'https://indexer.upgrade.thegraph.com/' indexer (IPv4: 104.18.40.31)
    let ip_network = "104.18.40.0/24".parse().expect("Invalid IP network");

    let host_blocklist = HashSet::from([ip_network]);
    let service = test_service_state(
        Default::default(), // No address blocklist
        host_blocklist,
        Default::default(), // No minimum versions
    );

    // Fetch and pre-process the network topology information
    let indexers_info = tokio::time::timeout(
        Duration::from_secs(10),
        fetch_and_pre_process_indexers_info(),
    )
    .await
    .expect("Topology fetch did not complete in time (10s)");

    // Require the pre-processed info to contain the "test indexer"
    assert!(
        indexers_info.keys().any(|addr| *addr == address),
        "Test indexer not found in the indexers info"
    );

    //* When
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        process_indexers_info(&service, indexers_info),
    )
    .await
    .expect("Topology processing did not complete in time (20s)");

    //* Then
    let indexers_processed_info = res.expect("Failed to process indexers info");

    // Assert that the blocked indexer is not present in the indexers processed info
    assert!(
        indexers_processed_info.keys().all(|addr| *addr != address),
        "Blocked indexer is present in the indexers processed info"
    );
}

#[test_with::env(IT_TEST_ARBITRUM_GATEWAY_URL, IT_TEST_ARBITRUM_GATEWAY_AUTH)]
#[tokio::test]
async fn fetch_indexers_info_and_block_all_indexers_by_agent_version() {
    init_test_tracing();

    //* Given
    // Set the minimum indexer agent version to block all indexers
    let min_versions = Some((
        Version::new(999, 999, 9999), // Indexer agent version
        Version::new(0, 0, 0),        // Graph node version
    ));

    let service = test_service_state(
        Default::default(), // No address blocklist
        Default::default(), // No host blocklist
        min_versions,
    );

    // Fetch and pre-process the network topology information
    let indexers_info = tokio::time::timeout(
        Duration::from_secs(10),
        fetch_and_pre_process_indexers_info(),
    )
    .await
    .expect("Topology fetch did not complete in time (10s)");

    //* When
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        process_indexers_info(&service, indexers_info),
    )
    .await
    .expect("Topology processing did not complete in time (20s)");

    //* Then
    // Assert the failure, as all indexers are blocked
    assert_matches!(res, Err(err) => {
        assert_eq!(err.to_string(), "no valid indexers found")
    });
}
