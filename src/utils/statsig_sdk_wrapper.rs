use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use statsig::StatsigUser;
use std::collections::HashMap;
use std::sync::Arc;

pub struct StatsigUserFactory {
    user: ArcSwap<StatsigUser>,
}

impl StatsigUserFactory {
    pub fn get(&self) -> Arc<StatsigUser> {
        Arc::clone(&self.user.load())
    }

    pub fn set(&self, custom_ids: Vec<(String, String)>) {
        let mut ids_map = HashMap::new();

        for (key, value) in custom_ids {
            ids_map.insert(
                key,
                std::env::var(value).unwrap_or_else(|_| "empty".to_string()),
            );
        }

        self.user
            .store(Arc::new(StatsigUser::with_custom_ids(ids_map)));
    }
}

impl Default for StatsigUserFactory {
    fn default() -> Self {
        let mut custom_ids = HashMap::new();
        custom_ids.insert(
            "podID".to_string(),
            std::env::var("HOSTNAME").unwrap_or_else(|_| "no_hostname_provided".to_string()),
        );

        Self {
            user: ArcSwap::new(Arc::new(StatsigUser::with_custom_ids(custom_ids))),
        }
    }
}

pub static STATSIG_USER_FACTORY: Lazy<Arc<StatsigUserFactory>> =
    Lazy::new(|| Arc::new(StatsigUserFactory::default()));
