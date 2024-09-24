use once_cell::sync::Lazy;
use statsig::StatsigUser;
use std::collections::HashMap;
use std::sync::Arc;

pub static STATSIG_USER: Lazy<Arc<StatsigUser>> = Lazy::new(|| {
    let mut custom_ids = HashMap::new();
    custom_ids.insert(
        "podID".to_string(),
        std::env::var("HOSTNAME").unwrap_or_else(|_| "no_hostname_provided".to_string()),
    );
    Arc::new(StatsigUser::with_custom_ids(custom_ids))
});
