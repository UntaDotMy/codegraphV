//! A minimal PCRE2 binding for UTF-8 `&str` subjects.
//!
//! The `pcre2` crate matches with no options, so PCRE2 re-validates the whole
//! subject's UTF-8 on every call: O(subject) per `exec`, which made scanning a
//! large file from many start positions quadratic. A `&str` is valid UTF-8 by
//! construction, so this binding passes `PCRE2_NO_UTF_CHECK`. That flag requires
//! the start offset to be a character boundary; any other offset reads as "no
//! match", as the `pcre2` crate's `BADUTFOFFSET` error did.

use pcre2_sys::*;
use std::ffi::c_void;
use std::ptr;

/// A compiled pattern (`PCRE2_UTF`, optionally `PCRE2_CASELESS`), JIT-compiled when available.
pub(crate) struct Regex {
    code: *mut pcre2_code_8,
    jit: bool,
    capture_names: Vec<Option<String>>,
}

// SAFETY: a compiled PCRE2 code object is read-only after compilation (JIT included)
// and PCRE2 documents it as safe to share between threads.
unsafe impl Send for Regex {}
unsafe impl Sync for Regex {}

impl Drop for Regex {
    fn drop(&mut self) {
        unsafe { pcre2_code_free_8(self.code) }
    }
}

fn error_message(code: i32) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { pcre2_get_error_message_8(code, buf.as_mut_ptr(), buf.len()) };
    if n < 0 {
        format!("PCRE2 error {code}")
    } else {
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    }
}

fn info_u32(code: *const pcre2_code_8, what: u32) -> u32 {
    let mut out: u32 = 0;
    let rc = unsafe { pcre2_pattern_info_8(code, what, &mut out as *mut u32 as *mut c_void) };
    assert_eq!(rc, 0, "pcre2_pattern_info({what}) failed");
    out
}

impl Regex {
    pub fn new(pattern: &str, caseless: bool) -> Result<Regex, String> {
        let mut options = PCRE2_UTF;
        if caseless {
            options |= PCRE2_CASELESS;
        }
        let mut err: i32 = 0;
        let mut off: usize = 0;
        let code = unsafe { pcre2_compile_8(pattern.as_ptr(), pattern.len(), options, &mut err, &mut off, ptr::null_mut()) };
        if code.is_null() {
            return Err(format!("PCRE2: error compiling pattern at offset {off}: {}", error_message(err)));
        }
        let jit = unsafe { pcre2_jit_compile_8(code, PCRE2_JIT_COMPLETE) } == 0;
        let mut re = Regex { code, jit, capture_names: Vec::new() };
        re.capture_names = re.read_capture_names();
        Ok(re)
    }

    /// One slot per group (0 = the whole match); `Some` for a named group.
    fn read_capture_names(&self) -> Vec<Option<String>> {
        let count = 1 + info_u32(self.code, PCRE2_INFO_CAPTURECOUNT) as usize;
        let mut names = vec![None; count];
        let name_count = info_u32(self.code, PCRE2_INFO_NAMECOUNT) as usize;
        if name_count == 0 {
            return names;
        }
        let entry_size = info_u32(self.code, PCRE2_INFO_NAMEENTRYSIZE) as usize;
        let mut table: *const u8 = ptr::null();
        let rc = unsafe { pcre2_pattern_info_8(self.code, PCRE2_INFO_NAMETABLE, &mut table as *mut *const u8 as *mut c_void) };
        assert_eq!(rc, 0, "pcre2_pattern_info(NAMETABLE) failed");
        // Each entry: a big-endian u16 group index, then the NUL-terminated name.
        let bytes = unsafe { std::slice::from_raw_parts(table, name_count * entry_size) };
        for entry in bytes.chunks_exact(entry_size) {
            let idx = ((entry[0] as usize) << 8) | entry[1] as usize;
            let name = &entry[2..];
            let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            names[idx] = Some(String::from_utf8_lossy(&name[..len]).into_owned());
        }
        names
    }

    pub fn capture_names(&self) -> &[Option<String>] {
        &self.capture_names
    }

    /// The number of groups, the whole match included.
    pub fn captures_len(&self) -> usize {
        self.capture_names.len()
    }

    /// A fresh match buffer; with JIT, it carries a JIT stack of up to `max_jit_stack` bytes.
    pub fn capture_locations(&self, max_jit_stack: usize) -> CaptureLocations {
        CaptureLocations::new(self, max_jit_stack)
    }

    /// Match from `start`, filling `locs`. A match error (a limit hit) reads as no match.
    pub fn captures_read_at(&self, locs: &mut CaptureLocations, subject: &str, start: usize) -> bool {
        if start > subject.len() || !subject.is_char_boundary(start) {
            return false;
        }
        // An empty subject still needs a dereferenceable pointer.
        static EMPTY: &[u8] = &[0];
        let ptr = if subject.is_empty() { EMPTY.as_ptr() } else { subject.as_ptr() };
        // SAFETY: `subject` is valid UTF-8 and `start` is a character boundary, which is
        // exactly the contract of PCRE2_NO_UTF_CHECK.
        let rc = unsafe { pcre2_match_8(self.code, ptr, subject.len(), start, PCRE2_NO_UTF_CHECK, locs.data, locs.ctx) };
        rc > 0
    }
}

/// A match-data block (the ovector) plus its match context and JIT stack.
pub(crate) struct CaptureLocations {
    data: *mut pcre2_match_data_8,
    ctx: *mut pcre2_match_context_8,
    stack: *mut pcre2_jit_stack_8,
    ovector: *const usize,
    pairs: usize,
}

// SAFETY: the buffer is owned exclusively and only used through `&mut` while matching.
unsafe impl Send for CaptureLocations {}

impl CaptureLocations {
    fn new(re: &Regex, max_jit_stack: usize) -> CaptureLocations {
        unsafe {
            let ctx = pcre2_match_context_create_8(ptr::null_mut());
            assert!(!ctx.is_null(), "failed to allocate a PCRE2 match context");
            let data = pcre2_match_data_create_from_pattern_8(re.code, ptr::null_mut());
            assert!(!data.is_null(), "failed to allocate a PCRE2 match data block");
            let mut stack = ptr::null_mut();
            if re.jit {
                stack = pcre2_jit_stack_create_8(max_jit_stack.min(4 << 10), max_jit_stack, ptr::null_mut());
                assert!(!stack.is_null(), "failed to allocate a PCRE2 JIT stack");
                pcre2_jit_stack_assign_8(ctx, None, stack as *mut c_void);
            }
            let ovector = pcre2_get_ovector_pointer_8(data);
            let pairs = pcre2_get_ovector_count_8(data) as usize;
            CaptureLocations { data, ctx, stack, ovector, pairs }
        }
    }

    /// Group `i`'s byte range from the last successful match, if it participated.
    pub fn get(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.pairs {
            return None;
        }
        let (start, end) = unsafe { (*self.ovector.add(2 * i), *self.ovector.add(2 * i + 1)) };
        if start == PCRE2_UNSET || end == PCRE2_UNSET {
            None
        } else {
            Some((start, end))
        }
    }
}

impl Drop for CaptureLocations {
    fn drop(&mut self) {
        unsafe {
            if !self.stack.is_null() {
                pcre2_jit_stack_free_8(self.stack);
            }
            pcre2_match_context_free_8(self.ctx);
            pcre2_match_data_free_8(self.data);
        }
    }
}
