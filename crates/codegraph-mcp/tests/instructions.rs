use codegraph_installer::upgrade::{format_update_notice, initialize_instructions};
use codegraph_mcp::tools::{INSTRUCTIONS, NO_INDEX_INSTRUCTIONS};

/// Claude Code truncates MCP `initialize` instructions past this many characters
/// (JavaScript string length, i.e. UTF-16 code units), dropping whatever comes last.
const HOST_INSTRUCTIONS_LIMIT: usize = 4096;

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

#[test]
fn initialize_instructions_fit_the_host_limit_with_an_update_notice() {
    let notice = format_update_notice("v0.0.3", "v10.20.30-rc.10");
    for (name, base) in [("indexed", INSTRUCTIONS), ("no-index", NO_INDEX_INSTRUCTIONS)] {
        for sent in [initialize_instructions(base, None), initialize_instructions(base, Some(&notice))] {
            let len = utf16_len(&sent);
            assert!(len <= HOST_INSTRUCTIONS_LIMIT, "{name} instructions are {len} chars, over {HOST_INSTRUCTIONS_LIMIT}");
        }
    }
}

#[test]
fn instructions_keep_the_essential_guidance() {
    for phrase in [
        "codegraphV_explore",
        "already Read",
        "⚠ Index may be stale",
        "⚠ Pending edits",
        "Did you mean",
        "codegraphV init",
        "projectPath",
        "dynamic-dispatch",
    ] {
        assert!(INSTRUCTIONS.contains(phrase), "indexed instructions lost: {phrase}");
    }
    for phrase in ["codegraphV_explore", "projectPath", ".codegraphV/", "codegraphV init"] {
        assert!(NO_INDEX_INSTRUCTIONS.contains(phrase), "no-index instructions lost: {phrase}");
    }
}

/// The Rust server never emits these TypeScript-only notices, so the instructions must not
/// teach agents to wait for them.
#[test]
fn instructions_name_no_notice_the_server_cannot_send() {
    for phrase in [
        "edited since the last index sync",
        "changed on disk after the last index sync",
        "EXPLORE_DEDUP",
        "Already sent earlier in this conversation",
    ] {
        assert!(!INSTRUCTIONS.contains(phrase), "instructions describe a notice the server never sends: {phrase}");
    }
}
