use dashmap::DashMap;
use tokio::sync::OnceCell;

use super::*;
use crate::datastore::idlist_membership::namespaces::IdListNamespaces;

type FetcherCell = Arc<OnceCell<Arc<RedisIdListMembershipFetcher>>>;

/// One dictionary cache and refresher per company, shared by all configured keys.
pub(super) struct CompanyMembershipPool {
    namespaces: Arc<IdListNamespaces>,
    schema: IdListKeySchema,
    pool: RedisPool,
    config: IdListMembershipConfig,
    fetchers: DashMap<String, FetcherCell>,
}

impl CompanyMembershipPool {
    pub(super) fn new(
        namespaces: Arc<IdListNamespaces>,
        schema: IdListKeySchema,
        pool: RedisPool,
        config: IdListMembershipConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            namespaces,
            schema,
            pool,
            config,
            fetchers: DashMap::new(),
        })
    }

    pub(super) fn resolver(
        self: &Arc<Self>,
        server_key: String,
    ) -> Arc<dyn IdListMembershipResolver> {
        Arc::new(CompanyKeyResolver {
            server_key,
            companies: Arc::clone(self),
        })
    }
}

struct CompanyKeyResolver {
    server_key: String,
    companies: Arc<CompanyMembershipPool>,
}

#[async_trait]
impl IdListMembershipResolver for CompanyKeyResolver {
    async fn resolve(
        &self,
        lookup_ids: Vec<String>,
    ) -> Result<HashMap<String, HashSet<String>>, IdListMembershipError> {
        let namespaces = &self.companies.namespaces;
        let scope = namespaces
            .namespace_for_key(&self.server_key)
            .zip(namespaces.visible_lists(&self.server_key));
        let Some((company_hash, _visible)) = scope.filter(|(_, visible)| !visible.is_empty())
        else {
            return Ok(lookup_ids
                .into_iter()
                .map(|id| (id, HashSet::new()))
                .collect());
        };
        // Release the DashMap guard before awaiting initialization. Failed initial
        // loads leave the cell empty, allowing the next request to retry.
        let cell = self
            .companies
            .fetchers
            .entry(company_hash.clone())
            .or_default()
            .clone();
        let fetcher = cell
            .get_or_try_init(|| {
                RedisIdListMembershipFetcher::new(
                    self.companies.schema.clone(),
                    company_hash.clone(),
                    self.companies.pool.clone(),
                    self.companies.config.clone(),
                )
            })
            .await?;
        let visible = namespaces
            .visible_lists(&self.server_key)
            .unwrap_or_default();
        let mut memberships = fetcher
            .fetch_visible(lookup_ids, &visible)
            .await?
            .into_map();

        // Re-read after Redis I/O so a manifest change while fetching cannot
        // return names that the key no longer sees. Missing scope returns no matches.
        let visible =
            if namespaces.namespace_for_key(&self.server_key).as_deref() == Some(&company_hash) {
                namespaces
                    .visible_lists(&self.server_key)
                    .unwrap_or_default()
            } else {
                HashSet::new()
            };
        for names in memberships.values_mut() {
            names.retain(|name| visible.contains(name));
        }
        Ok(memberships)
    }
}

#[cfg(test)]
mod tests;
