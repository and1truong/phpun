//! Thin wrapper over vendored PCRE2 (vivacity-pcre2-sys, statically
//! linked, symbols prefixed — no system libpcre2 dependency).
//!
//! PHP's preg_* functions ARE PCRE2, so this is used for every pattern
//! the `regex` crate can't express — backtracking verbs
//! (`(*SKIP)(*F)`, `(*MARK:x)`), recursion `(?-n)`, lookbehind,
//! backreferences — and for `(*MARK)` data the high-level pcre2 crate
//! doesn't expose. `unsafe` is confined to this module; all pointers
//! are non-null-checked and freed in `Drop`.

use pcre2_sys::*;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;

/// One match: group byte spans (`spans[0]` = whole match; `None` =
/// unset group) plus the `(*MARK:...)` verb payload if one fired.
pub struct PcreMatch {
    pub spans: Vec<Option<(usize, usize)>>,
    pub mark: Option<String>,
}

pub struct PcreRe {
    code: *mut pcre2_code_8,
    /// group index -> name (index 0 = whole match, never named)
    names: Vec<Option<String>>,
}

impl Drop for PcreRe {
    fn drop(&mut self) {
        unsafe { pcre2_code_free_8(self.code) }
    }
}

impl PcreRe {
    pub fn captures_len(&self) -> usize {
        self.names.len()
    }

    pub fn group_name(&self, g: usize) -> Option<String> {
        self.names.get(g).cloned().flatten()
    }

    /// All leftmost matches, PHP `preg_match_all` order.
    pub fn match_all(&self, subject: &[u8]) -> Vec<PcreMatch> {
        unsafe {
            let md = pcre2_match_data_create_from_pattern_8(self.code, ptr::null_mut());
            if md.is_null() {
                return Vec::new();
            }
            let ovc = pcre2_get_ovector_count_8(md) as usize;
            let mut out = Vec::new();
            let mut offset = 0usize;
            let len = subject.len();
            while offset <= len {
                let rc = pcre2_match_8(
                    self.code,
                    subject.as_ptr(),
                    len,
                    offset,
                    0,
                    md,
                    ptr::null_mut(),
                );
                if rc <= 0 {
                    break;
                }
                let ov = pcre2_get_ovector_pointer_8(md);
                let mut spans = vec![None; self.names.len().max(ovc)];
                for (i, s) in spans.iter_mut().enumerate().take(ovc) {
                    let (a, b) = (*ov.add(2 * i), *ov.add(2 * i + 1));
                    if a != usize::MAX && b != usize::MAX {
                        *s = Some((a, b));
                    }
                }
                let mark = {
                    let m = pcre2_get_mark_8(md);
                    if m.is_null() {
                        None
                    } else {
                        Some(
                            CStr::from_ptr(m.cast::<std::os::raw::c_char>())
                                .to_string_lossy()
                                .into_owned(),
                        )
                    }
                };
                let (s0, e0) = spans.first().copied().flatten().unwrap_or((offset, offset));
                out.push(PcreMatch { spans, mark });
                // Empty match: advance past the current position like
                // pcre2demo (keeps progress, keeps UTF-8 alignment for
                // /u patterns since PCRE2 offsets stay on boundaries).
                offset = if e0 > s0 { e0 } else { e0 + 1 };
            }
            pcre2_match_data_free_8(md);
            out
        }
    }
}

/// Compile a PCRE2 pattern (without delimiters — options already
/// inlined as `(?imsx)` by the caller). Returns None on compile error.
pub fn compile(src: &str) -> Option<PcreRe> {
    unsafe {
        let mut errcode: std::os::raw::c_int = 0;
        let mut erroff: usize = 0;
        let code = pcre2_compile_8(
            src.as_ptr(),
            src.len(),
            0,
            &mut errcode,
            &mut erroff,
            ptr::null_mut(),
        );
        if code.is_null() {
            return None;
        }
        let mut ncap: u32 = 0;
        pcre2_pattern_info_8(
            code,
            PCRE2_INFO_CAPTURECOUNT,
            (&mut ncap as *mut u32).cast::<c_void>(),
        );
        let mut names = vec![None; (ncap as usize) + 1];
        let mut ntable_count: u32 = 0;
        pcre2_pattern_info_8(
            code,
            PCRE2_INFO_NAMECOUNT,
            (&mut ntable_count as *mut u32).cast::<c_void>(),
        );
        if ntable_count > 0 {
            let mut esize: u32 = 0;
            let mut table: PCRE2_SPTR8 = ptr::null();
            pcre2_pattern_info_8(
                code,
                PCRE2_INFO_NAMEENTRYSIZE,
                (&mut esize as *mut u32).cast::<c_void>(),
            );
            pcre2_pattern_info_8(
                code,
                PCRE2_INFO_NAMETABLE,
                (&mut table as *mut PCRE2_SPTR8).cast::<c_void>(),
            );
            for i in 0..ntable_count as usize {
                let entry = table.add(i * esize as usize);
                // first two bytes = group number, then NUL-terminated name
                let num = u16::from_ne_bytes([*entry, *entry.add(1)]) as usize;
                let name_ptr = entry.add(2).cast::<std::os::raw::c_char>();
                let name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                if num < names.len() {
                    names[num] = Some(name);
                }
            }
        }
        Some(PcreRe { code, names })
    }
}
