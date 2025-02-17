use crate::{DhtClient, DhtConfig, NameResolver, NameSystem};
use anyhow::Result;
use async_trait::async_trait;
use libp2p::Multiaddr;
use noosphere_core::{
    authority::generate_ed25519_key,
    data::{Did, LinkRecord},
};
use std::collections::HashMap;
use tokio::sync::Mutex;
use ucan::store::UcanJwtStore;

/// An in-process network of [NameSystem] nodes for testing.
pub struct NameSystemNetwork {
    nodes: Vec<NameSystem>,
    address: Multiaddr,
}

impl NameSystemNetwork {
    /// [NameSystem] nodes in the network.
    pub fn nodes(&self) -> &Vec<NameSystem> {
        &self.nodes
    }

    /// [NameSystem] nodes in the network.
    pub fn nodes_mut(&mut self) -> &mut Vec<NameSystem> {
        &mut self.nodes
    }

    /// Get reference to `index` [NameSystem] node.
    pub fn get(&self, index: usize) -> Option<&NameSystem> {
        self.nodes.get(index)
    }

    /// Get mutable reference to `index` [NameSystem] node.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut NameSystem> {
        self.nodes.get_mut(index)
    }

    /// An address of a node in the network to join.
    pub fn address(&self) -> &Multiaddr {
        &self.address
    }

    /// Generates a DHT network bootstrap node with `node_count`
    /// [NameSystem]s connected, each with a corresponding owner sphere.
    /// Useful for tests. All nodes share an underlying (cloned) store
    /// that may share state.
    pub async fn generate<S: UcanJwtStore + Clone + 'static>(
        node_count: usize,
        store: Option<S>,
    ) -> Result<Self> {
        let mut bootstrap_address: Option<Multiaddr> = None;
        let mut nodes = vec![];
        for _ in 0..node_count {
            let key = generate_ed25519_key();
            let node = NameSystem::new(&key, DhtConfig::default(), store.clone())?;
            let address = node.listen("/ip4/127.0.0.1/tcp/0".parse()?).await?;
            if let Some(address) = bootstrap_address.as_ref() {
                node.add_peers(vec![address.to_owned()]).await?;
            } else {
                bootstrap_address = Some(address);
            }
            nodes.push(node);
        }
        Ok(NameSystemNetwork {
            nodes,
            address: bootstrap_address.unwrap(),
        })
    }
}

pub struct KeyValueNameResolver {
    store: Mutex<HashMap<Did, LinkRecord>>,
}

impl KeyValueNameResolver {
    pub fn new() -> Self {
        KeyValueNameResolver {
            store: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for KeyValueNameResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NameResolver for KeyValueNameResolver {
    async fn publish(&self, record: LinkRecord) -> Result<()> {
        let mut store = self.store.lock().await;
        let did_id = Did(record.sphere_identity().into());
        store.insert(did_id, record);
        Ok(())
    }

    async fn resolve(&self, identity: &Did) -> Result<Option<LinkRecord>> {
        let store = self.store.lock().await;
        Ok(store.get(identity).map(|record| record.to_owned()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::name_resolver_tests;
    async fn before_name_resolver_tests() -> Result<KeyValueNameResolver> {
        Ok(KeyValueNameResolver::new())
    }
    name_resolver_tests!(KeyValueNameResolver, before_name_resolver_tests);
}
