# Native integration preservation

Instruction updates are restricted to the managed CodeGraphV block. New blocks
carry an internal append-boundary comment so removing them can preserve empty
files, original line endings and files without a final newline. It is not a
backup: edits outside the block survive uninstall. Pre-existing legacy blocks
cannot recover append-boundary information that was never recorded. Duplicate,
nested or incomplete markers produce a diagnostic before target writes; code
examples inside Markdown fences or indented blocks are not managed instructions.

Local Claude setup uses an existing AGENTS.md when neither root nor `.claude/`
CLAUDE.md exists. This avoids introducing a CLAUDE.md that shadows the existing
AGENTS.md under the current Claude loading contract. Older hosts without direct
AGENTS.md loading need separately configured instruction imports. Existing
shadowing layouts are not automatically relocated.

Claude hooks use the current direct-exec contract: `command` is the executable
path and `args` is `["prompt-hook"]`. A host supporting this contract is required;
older shell-only hosts have not been certified. This avoids shell expansion and
does not require Git Bash on Windows. Exact legacy shell commands are migrated;
commands that merely mention CodeGraphV are preserved.

`install --refresh` preserves the existing permission and hook choices.
`--no-permissions` explicitly removes CodeGraphV permissions.
`--prompt-hook preserve|on|off` independently controls hook installation. The hook
accepts at most 1 MiB of input and exits silently after a default two-second
budget. `CODEGRAPHV_PROMPT_HOOK_TIMEOUT_MS` selects 100–5000 milliseconds.

JSON/TOML MCP launch updates preserve other per-server preferences. Malformed
Claude hook and permission structures are rejected before writing. Installation
journals the complete plan and uses an OS lock shared by installation and recovery.
The empty `~/.codegraphV/install.lock` file remains after uninstall so all processes
lock the same file. Interrupted writes are restored only if their contents still
match the installation's output; failed recovery retains its journal for retry.
Unreadable or unknown journals require inspection rather than silent deletion.
Content checks before each write detect intervening edits, but cannot atomically
exclude an unrelated editor during the final comparison/rename interval. Live-host,
filesystem metadata and cross-platform recovery coverage remain release gates.

Current host contracts consulted:
[Claude hooks](https://code.claude.com/docs/en/hooks#exec-form-and-shell-form),
[Claude instruction loading](https://code.claude.com/docs/en/memory#when-claude-code-reads-agentsmd).
Native fixtures validate emitted configuration and execution; live-host and
cross-platform certification remain separate gates.
