use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, BlockNumber};
use eventuals::{self, Eventual, EventualExt as _, EventualWriter, Ptr};
use serde::Deserialize;
use serde_with::serde_as;
use thegraph_core::{
    client as subgraph_client,
    types::{DeploymentId, SubgraphId},
};
use tokio::sync::Mutex;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subgraph {
    pub id: SubgraphId,
    pub id_on_l2: Option<SubgraphId>,
    pub versions: Vec<SubgraphVersion>,
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub network: Option<String>,
    #[serde_as(as = "Option<serde_with::DisplayFromStr>")]
    pub start_block: Option<BlockNumber>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubgraphVersion {
    pub subgraph_deployment: SubgraphDeployment,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubgraphDeployment {
    #[serde(rename = "ipfsHash")]
    pub id: DeploymentId,
    #[serde(rename = "indexerAllocations")]
    pub allocations: Vec<Allocation>,
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub transferred_to_l2: bool,
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Allocation {
    pub id: Address,
    pub indexer: Indexer,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub allocated_tokens: u128,
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Indexer {
    pub id: Address,
    pub url: Option<String>,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub staked_tokens: u128,
}

pub struct Client {
    subgraph_client: subgraph_client::Client,
    subgraphs: EventualWriter<Ptr<Vec<Subgraph>>>,
    // TODO: remove when L2 subgraph transfer support is on mainnet network subgraphs
    l2_transfer_support: bool,
}

impl Client {
    pub async fn create(
        subgraph_client: subgraph_client::Client,
        l2_transfer_support: bool,
    ) -> Eventual<Ptr<Vec<Subgraph>>> {
        let (subgraphs_tx, subgraphs_rx) = Eventual::new();
        let client = Arc::new(Mutex::new(Client {
            subgraph_client,
            subgraphs: subgraphs_tx,
            l2_transfer_support,
        }));

        // 4e072dfe-5cb3-4f86-80f6-b64afeb9dcb2
        eventuals::timer(Duration::from_secs(30))
            .pipe_async(move |_| {
                let client = client.clone();
                async move {
                    let mut client = client.lock().await;
                    if let Err(poll_subgraphs_err) = client.poll_subgraphs().await {
                        tracing::error!(%poll_subgraphs_err);
                    }
                }
            })
            .forever();

        subgraphs_rx
    }

    #[allow(clippy::obfuscated_if_else)]
    async fn poll_subgraphs(&mut self) -> Result<(), String> {
        // last allocation is latest by indexing: 9936786a-e286-45f3-9190-8409d8389e88
        let query = format!(
            r#"
            subgraphs(
                block: $block
                orderBy: id, orderDirection: asc
                first: $first
                where: {{
                    id_gt: $last
                    entityVersion: 2
                    {}
                }}
            ) {{
                id
                {}
                versions(orderBy: version, orderDirection: asc) {{
                    subgraphDeployment {{
                        ipfsHash
                        manifest {{
                            network
                            startBlock
                        }}
                        indexerAllocations(
                            first: 100
                            orderBy: createdAt, orderDirection: asc
                            where: {{ status: Active }}
                        ) {{
                            id
                            allocatedTokens
                            indexer {{
                                id
                                url
                                stakedTokens
                            }}
                        }}
                        {}
                    }}
                }}
            }}
        "#,
            self.l2_transfer_support
                .then_some("")
                .unwrap_or("active: true"),
            self.l2_transfer_support.then_some("idOnL2").unwrap_or(""),
            self.l2_transfer_support
                .then_some("transferredToL2")
                .unwrap_or(""),
        );

        let subgraphs = self
            .subgraph_client
            .paginated_query::<Subgraph>(query, 200)
            .await?;

        if subgraphs.is_empty() {
            return Err("Discarding empty update (subgraph_deployments)".to_string());
        }

        self.subgraphs.write(Ptr::new(subgraphs));
        Ok(())
    }
}
