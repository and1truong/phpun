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
    /// compiled with PCRE2_UTF — matches must stay on char boundaries
    utf8: bool,
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

    /// All leftmost matches, PHP `preg_match_all` order. Returns the
    /// matches plus 0, or the negative PCRE2 error code (backtrack /
    /// depth limit, bad UTF-8, ...) that ended the scan.
    pub fn match_all(
        &self,
        subject: &[u8],
        match_limit: u32,
        depth_limit: u32,
    ) -> (Vec<PcreMatch>, i32) {
        unsafe {
            let md = pcre2_match_data_create_from_pattern_8(self.code, ptr::null_mut());
            if md.is_null() {
                return (Vec::new(), 0);
            }
            let mctx = pcre2_match_context_create_8(ptr::null_mut());
            if !mctx.is_null() {
                pcre2_set_match_limit_8(mctx, match_limit);
                pcre2_set_depth_limit_8(mctx, depth_limit);
            }
            let ovc = pcre2_get_ovector_count_8(md) as usize;
            let mut out = Vec::new();
            let mut err = 0i32;
            let mut offset = 0usize;
            let len = subject.len();
            while offset <= len {
                let rc = pcre2_match_8(self.code, subject.as_ptr(), len, offset, 0, md, mctx);
                if rc == 0 || rc == PCRE2_ERROR_NOMATCH {
                    break;
                }
                if rc < 0 {
                    err = rc;
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
                if e0 > s0 {
                    out.push(PcreMatch { spans, mark });
                    offset = e0;
                    continue;
                }
                // Empty match — PHP's global-scan rule: record it, then
                // retry anchored+notempty at the same offset; on no match
                // move past one character (UTF-8-aware under /u).
                out.push(PcreMatch { spans, mark });
                let rc2 = pcre2_match_8(
                    self.code,
                    subject.as_ptr(),
                    len,
                    offset,
                    PCRE2_NOTEMPTY_ATSTART | PCRE2_ANCHORED,
                    md,
                    mctx,
                );
                if rc2 > 0 {
                    let ov = pcre2_get_ovector_pointer_8(md);
                    let (a1, b1) = (*ov, *ov.add(1));
                    offset = if a1 != usize::MAX { b1 } else { e0 + 1 };
                } else if rc2 == 0 || rc2 == PCRE2_ERROR_NOMATCH {
                    offset = e0
                        + if self.utf8 && e0 < len {
                            let mut n = 1usize;
                            while e0 + n < len && (subject[e0 + n] & 0xC0) == 0x80 {
                                n += 1;
                            }
                            n
                        } else {
                            1
                        };
                } else {
                    err = rc2;
                    break;
                }
            }
            if !mctx.is_null() {
                pcre2_match_context_free_8(mctx);
            }
            pcre2_match_data_free_8(md);
            (out, err)
        }
    }
}

/// Compile a PCRE2 pattern (without delimiters — options already
/// inlined as `(?imsx)` or passed in `options` by the caller).
/// Err is the engine's message plus the byte offset, PHP style.
pub fn compile(src: &str, options: u32, extra_options: u32) -> Result<PcreRe, String> {
    unsafe {
        let mut errcode: std::os::raw::c_int = 0;
        let mut erroff: usize = 0;
        let cctx = if extra_options != 0 {
            let c = pcre2_compile_context_create_8(ptr::null_mut());
            pcre2_set_compile_extra_options_8(c, extra_options);
            c
        } else {
            ptr::null_mut()
        };
        let code = pcre2_compile_8(
            src.as_ptr(),
            src.len(),
            options,
            &mut errcode,
            &mut erroff,
            cctx,
        );
        if !cctx.is_null() {
            pcre2_compile_context_free_8(cctx);
        }
        if code.is_null() {
            let mut buf = [0u8; 256];
            let n = pcre2_get_error_message_8(errcode, buf.as_mut_ptr().cast(), buf.len());
            let msg = if n > 0 {
                String::from_utf8_lossy(&buf[..n as usize]).into_owned()
            } else {
                format!("error {}", errcode)
            };
            return Err(format!("{} at offset {}", msg, erroff));
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
                let num = u16::from_be_bytes([*entry, *entry.add(1)]) as usize;
                let name_ptr = entry.add(2).cast::<std::os::raw::c_char>();
                let name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                if num < names.len() {
                    names[num] = Some(name);
                }
            }
        }
        Ok(PcreRe {
            code,
            names,
            utf8: options & PCRE2_UTF != 0,
        })
    }
}
