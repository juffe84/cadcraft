Detail requests test smaller ranges inside each LAZ block so a sparse strip crossing the
viewport's bounding box does not trigger decompression when none of its points can be visible.
The GPU startup log reports the active adapter and the point-cloud indexer reports its worker count.
The overview and detail use separate GPU buffers, so updates keep the overview visible.
