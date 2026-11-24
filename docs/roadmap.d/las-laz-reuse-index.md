Large desktop LAS and LAZ imports reuse a bounded index for the most recently indexed file.
An initial scan can recover detail from blocks it has already mapped. Uncached detail reads
stop at the last overlapping range in each block. On Unix, workers read and decode their
blocks in parallel from the authorized file handle.
