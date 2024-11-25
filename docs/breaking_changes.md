# Breaking Changes

This document acts a historical tracker of all breaking changes that are introduced by version.

# v3.0.0 Breaking Changes

## Introduced new key schema
New key schema matches against the one that will be generated from SDKs, but also now includes an encoding type.

**Migration Strategy**: If you use an external cache, first enable --double-write-cache-for-legacy-key, then update the data adapter code in your SDK to use: https://github.com/statsig-io/statsig-forward-proxy/blob/main/src/datastore/caching/redis_cache.rs#L249-L259 , and then disable --double-write-cache-for-legacy-key.

This legacy key will use the v0.x.x and v1.x.x logic. Not the 2.x.x.

# v2.0.0 Breaking Changes

## Nginx Support
We introduced this to improve performance at scale for larger payloads at high qps

**Migration Strategy**: This will cause a reduction in overall request volume due to our nginx configuration serving from its internal cache for a majority of requests.

## clear_datastore_on_unauthorized
This was changed from clear_external_datastore_on_unauthorized which use to only clear the external datastores, but has been updated to also control the behavior of the internal datastore.

The rational for this was that if we have a SEV related to authorization, we don't want to clear the internal store either. The cache will then be cleared on restart.

We understand this is not the ideal solution, but a stop gap measure to ensure we don't risk unavailability. We are following up with another change soon, but wanted to mitigate this risk first.

**Migration Strategy**: If you delete a key and still see it served, but don't want it to be available anymore, just restart the service.

## Deprecated idlist
We are moving to a more sustainable solution for serving IDlist payloads, as a result, we no longer plan to serve IDlist from the proxy.

**Migration Strategy**: On the SDK side, if you overrode the API to to send idlist traffic to the forward proxy, please remove that setting before upgrading.

## Removed InMemoryCache
This cache was only used for development, but customers were selecting it by accident.

**Migration Strategy**: Choose a different cache, for equivalent behavior use "disabled"

## Introduced new key schema
We are moving to a new key schema that will allow us to move key generation into the SDKs instead of requiring each user to define it themselves.

**Migration Strategy**: If you use an external cache, first enable --double-write-cache-for-legacy-key, then update the data adapter code in your SDK to use: https://github.com/statsig-io/statsig-forward-proxy/blob/main/src/datastore/caching/redis_cache.rs#L231C5-L238C6 , and then disable --double-write-cache-for-legacy-key.
