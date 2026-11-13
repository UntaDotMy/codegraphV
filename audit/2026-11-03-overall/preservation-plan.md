# Preserve the user's instructions, hooks and configuration

Requested behavior: installation adds CodeGraphV to the user's existing setup without replacing their instructions, hooks or preferences. Reinstall updates only the existing CodeGraphV-owned section. Append does not mean adding duplicate sections on every run.

## Research and current behavior

[AGENTS.md](https://agents.md/) is project guidance for coding agents. [Claude's memory documentation](https://code.claude.com/docs/en/memory) describes user/project instructions and their loading rules. [Claude's hook reference](https://code.claude.com/docs/en/hooks) describes structured hook groups; merging those objects is different from appending arbitrary text to JSON.

Current native implementation uses `markers::upsert` for instructions and structured config editors for settings. For a normal nonempty CLAUDE.md, AGENTS.md or GEMINI.md, first install preserves existing text and appends a fenced block. Reinstall/refresh replaces that owned block rather than the entire document. Claude's ordinary existing UserPromptSubmit groups and Stop hooks survive the tested install/refresh/uninstall sequence. The writer may atomically replace the filesystem entry; the preservation requirement concerns user content as well as links/metadata, not whether it literally calls an append syscall.

Fresh probes in `lifecycle-probes.json` cover four local targets (Claude, Codex, OpenCode, Gemini), each with LF, CRLF, missing-final-newline and whitespace-only instruction files: 16 fixture sequences. Normal LF/CRLF content and exact round trips pass; no-final-newline files preserve the prefix on install but change on uninstall; whitespace-only originals do not preserve exact bytes/existence. All 16 refreshes are byte-identical to first install and have one managed block. A separate ordinary hook-sibling case preserves user group properties, Stop hook, unrelated settings and permission entries semantically.

Additional failures: fenced example markers are overwritten (A33); substring hook ownership removes unrelated commands (A02); shared AGENTS.md guidance is removed while OpenCode still needs it (A03); refresh restores opted-out permission (A01). These are concrete limitations on any blanket claim that installation never overwrites user content.

There is also an instruction-loading risk: current Claude defaults to AGENTS.md only when no project CLAUDE.md is present, and `.claude/CLAUDE.md` counts. Creating that file can hide the user's AGENTS.md from the host without modifying its bytes (A35). Test supported host versions and modes; do not assume one placement works for all versions. These conclusions are based on source plus current documentation, not a new live-host test.

## Required implementation contract

1. **First install:** retain all user-authored instruction bytes, add one clearly owned block at an appropriate location, preserve actual host loading of pre-existing instructions. Never rename/delete an existing instruction file to make room.
2. **Reinstall/refresh:** edit only the owned block and owned config keys. Do not alter surrounding headings, examples, imports, rules, other products' markers, permissions or hook choices. Changes inside an explicitly managed block may be refreshed; document that boundary.
3. **Hooks:** insert one owned hook into the existing event structure; retain other events, matchers, timeouts, async settings, group fields and commands. Determine ownership exactly, not by substring. Malformed existing structures produce a no-write diagnostic, not wholesale replacement with an empty object/array.
4. **Uninstall:** remove only owned additions, retaining guidance while another installed integration consumes it. Preserve files that existed before install, including empty files. Delete newly created files only if no user content remains. Preserve later user edits; do not blindly copy the preinstall backup over them.
5. **MCP settings:** merge managed command/args fields while preserving user-owned per-server environment, timeouts, enabled-tool selections and host-specific preferences unless a documented migration requires a precise change. Current editors can replace whole owned entries/tables when command/args change; add explicit tests before promising preference preservation.
6. **Filesystem behavior:** preserve symlink targets, encoding/BOM, line endings and supported permissions/ACLs. Reject unreadable or undecodable input without modifying any part of the operation. Test hard links and decide whether to preserve or refuse; atomic rename alone does not preserve hard-link identity.
7. **Failure/concurrency:** preflight the complete operation, show planned ownership changes, use recoverable writes and conflict detection. An intervening user edit must not be overwritten by a stale read. Keep dry-run genuinely write-free.

## Acceptance matrix for the implementation agent

| Dimension | Required cases |
|---|---|
| Scope/targets | Global and local; profile overrides; Claude/Codex/OpenCode/Gemini; shared local AGENTS.md; original CodeGraph side by side |
| Instruction content | LF/CRLF, BOM, Unicode, no final newline, empty/whitespace-only, large file, trailing spaces, user text before/after managed block |
| Marker ambiguity | Missing end, reversed/nested/duplicate markers, inline examples, backtick/tilde fences, quoted examples, old brand markers; ambiguous input must preserve user text |
| Host semantics | Root CLAUDE.md only, AGENTS.md only, both, imported AGENTS.md, nested repo/worktree and host instruction-mode settings; compare loaded guidance before/after |
| Hook content | Multiple groups per event, multiple hooks per group, unfamiliar fields, other events, `echo`/comments mentioning CodeGraphV, old/new exact commands, direct exec form |
| Config edits | Comments/order, unrelated MCP servers, extra settings within the managed server, malformed JSON/TOML/YAML, symlinked config, user edits between install and removal |
| Lifecycle | Install twice, refresh, upgrade refresh, disable hook/permission then refresh, remove one shared consumer, remove final consumer, reinstall after interrupted operation |
| Failures | Read/write denial, full disk, partial write, rename failure, concurrent install, concurrent user editor; verify no silent user-data loss |

Promote the audit probes into durable native regression tests with explicit expected preservation. Audit scripts report failures; they are not replacement product tests. Link A01–A04/A08/A11/A33–A35 to this matrix when marking fixes complete.
