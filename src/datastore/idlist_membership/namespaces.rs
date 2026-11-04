use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;

use super::{company_ids::CompanyIdStore, hash_company_id};
use crate::datastore::id_list_manifest::IdListManifest;

/// Shared company identity and each configured key's latest successful manifest.
pub struct IdListNamespaces {
    pub company_ids: Arc<CompanyIdStore>,
    pub(crate) server_keys: Arc<Vec<String>>,
    pub(crate) manifests: Arc<ArcSwap<HashMap<String, IdListManifest>>>,
}

#[derive(Debug, Default)]
pub(crate) struct CompanyManifest {
    pub lists: IdListManifest,
    pub incomplete: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NamespaceCollision {
    pub namespace: String,
    pub company_ids: Vec<String>,
}

impl IdListNamespaces {
    pub fn new(mut server_keys: Vec<String>) -> Self {
        server_keys.sort();
        server_keys.dedup();
        Self {
            company_ids: Arc::new(CompanyIdStore::new(server_keys.clone())),
            server_keys: Arc::new(server_keys),
            manifests: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    pub fn namespace_for_key(&self, key: &str) -> Option<String> {
        self.company_ids
            .get_company_id(key)
            .map(|id| hash_company_id(&id))
    }

    pub fn visible_lists(&self, key: &str) -> Option<HashSet<String>> {
        self.manifests
            .load()
            .get(key)
            .map(|lists| lists.keys().cloned().collect())
    }

    /// Plan once per company using the union, so a key cannot remove another
    /// key's lists. Missing snapshots are not authoritative empty manifests.
    pub(crate) fn company_manifests(
        &self,
    ) -> Result<(BTreeMap<String, CompanyManifest>, u64), Vec<NamespaceCollision>> {
        // Use the same discovered identities for validation and planning, even if
        // discovery updates while this tick runs. Check keys with missing manifests too.
        let identities: Vec<_> = self
            .server_keys
            .iter()
            .map(|key| (key, self.company_ids.get_company_id(key)))
            .collect();
        let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (_, company_id) in &identities {
            if let Some(company_id) = company_id {
                owners
                    .entry(hash_company_id(company_id))
                    .or_default()
                    .insert(company_id.clone());
            }
        }
        let collisions: Vec<_> = owners
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(namespace, ids)| NamespaceCollision {
                namespace,
                company_ids: ids.into_iter().collect(),
            })
            .collect();
        if !collisions.is_empty() {
            return Err(collisions);
        }

        let snapshots = self.manifests.load();
        let mut companies: BTreeMap<String, CompanyManifest> = BTreeMap::new();
        let mut unavailable = 0;
        let mut unknown_company = false;
        for (key, company_id) in identities {
            let Some(company_id) = company_id else {
                unavailable += 1;
                unknown_company = true;
                continue;
            };
            let namespace = hash_company_id(&company_id);
            let company = companies.entry(namespace).or_default();
            let Some(manifest) = snapshots.get(key) else {
                unavailable += 1;
                company.incomplete = true;
                continue;
            };
            for (name, entry) in manifest {
                let replace = company.lists.get(name).is_none_or(|previous| {
                    if previous.file_id == entry.file_id {
                        (entry.size, entry.creation_time) > (previous.size, previous.creation_time)
                    } else {
                        (entry.creation_time, &entry.file_id)
                            > (previous.creation_time, &previous.file_id)
                    }
                });
                if replace {
                    company.lists.insert(name.clone(), entry.clone());
                }
            }
        }
        // An unresolved key could belong to any company. Load known lists, but
        // do not remove data or declare a complete namespace until it resolves.
        if unknown_company {
            for company in companies.values_mut() {
                company.incomplete = true;
            }
        }
        Ok((companies, unavailable))
    }
}

#[cfg(test)]
mod tests;
