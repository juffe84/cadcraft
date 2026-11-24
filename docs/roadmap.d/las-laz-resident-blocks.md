Sampled desktop point clouds stream full-resolution chunks in parallel and keep nearby
chunks when the view moves. The GPU retains buffers for overlapping views and spreads new
uploads across frames. Dense orbit views draw fewer instances while moving. Point snaps
use the source coordinates from the loaded chunks.
