# Native Git hook policy

The Git hook helpers in `src/git_hooks.rs` are library utilities. The native CLI
does not install Git hooks during `install`, `init`, `index`, or upgrade, and
`uninit` does not remove hooks. Existing user Git hooks are therefore not changed
by these commands. Use the native watcher or explicit `codegraphV sync` for
automatic-session or manual synchronization respectively.

Calling the library helpers is an explicit embedding decision. Their existing
tests cover managed blocks, sibling hooks, worktrees and custom hook paths; they
do not establish a production CLI hook lifecycle. Wiring that lifecycle requires
an explicit opt-in, an absolute executable path, shared subproject ownership and
active-runtime cleanup tests before it is advertised as supported.
