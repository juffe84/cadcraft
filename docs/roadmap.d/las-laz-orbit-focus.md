Orbit centers detail loading on the visible surface after camera movement settles. The detail
worker reads nearer blocks first, skips offscreen point ranges when building a sample, and keeps
nearby block data for subsequent views even when a larger request cannot fit in the cache.
