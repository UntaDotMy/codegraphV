#!/usr/bin/env python
"""Stand-in for the two `jq` invocations that no-cli-shim.sh's hook uses.

`jq` is not installed on this machine, and the hook needs exactly two things: the
`tool_input.command` string of a PreToolUse event, and `-n --arg m VALUE` to build
the denial message object. Both are implemented here so the shared shim runs
unmodified. Any other jq invocation fails loudly, so a future edit to the hook
cannot quietly get an answer this does not compute.
"""
import json
import sys

READ_QUERY = '.tool_input.command // empty'
DENY_FILTER = '{reason:$m, hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$m}}'


def read_command() -> int:
    try:
        event = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0
    value = (event.get('tool_input') or {}).get('command')
    if isinstance(value, str):
        sys.stdout.write(value)
    return 0


def deny_message(argv: list[str]) -> int:
    if argv[:2] != ['--arg', 'm'] or len(argv) != 4 or argv[3] != DENY_FILTER:
        sys.exit(f'ab-rust-builds: this shim implements only `jq -r {READ_QUERY!r}` and `jq -n --arg m .. {DENY_FILTER!r}`')
    message = argv[2]
    out = {
        'reason': message,
        'hookSpecificOutput': {
            'hookEventName': 'PreToolUse',
            'permissionDecision': 'deny',
            'permissionDecisionReason': message,
        },
    }
    sys.stdout.write(json.dumps(out))
    return 0


def main() -> int:
    argv = sys.argv[1:]
    if argv == ['-r', READ_QUERY]:
        return read_command()
    if argv and argv[0] == '-n':
        return deny_message(argv[1:])
    sys.exit(f'ab-rust-builds: unsupported jq invocation {argv!r}')


if __name__ == '__main__':
    sys.exit(main())
