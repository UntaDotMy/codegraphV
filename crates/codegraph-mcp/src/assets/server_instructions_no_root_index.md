# CodeGraphV

A queue-full retry response means the request was not executed; retry it after the server is less busy.

There is no default index here. Use `codegraphV_explore` with `projectPath`
to query an existing `.codegraphV/` index in another project or subdirectory.
It returns numbered source and recorded call paths; treat source as already Read.
For an unindexed project, use built-in tools. Indexing is the user's decision:
mention `codegraphV init` if useful, but never run it automatically. Newly created
indexes are detected without restarting the server.
