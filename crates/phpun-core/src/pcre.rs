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
    pub utf8: bool,
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

    /// All leftmost matches starting at `start_offset`, PHP
    /// `preg_match_all` order. Returns the matches plus 0, or the
    /// negative PCRE2 error code (backtrack / depth limit, bad UTF-8,
    /// ...) that ended the scan. The full subject is passed (not a
    /// slice) so lookbehind / `\b` see the context before the offset.
    /// `global` mirrors PHP's preg_match vs preg_match_all: when
    /// false the scan stops after the first recorded match.
    /// `first_opts` are the match options for the FIRST `pcre2_match`
    /// call only (PHP passes `PCRE2_NO_UTF_CHECK` when the subject is
    /// already known-valid UTF-8); every later call in the global
    /// loop uses `PCRE2_NO_UTF_CHECK` unconditionally.
    pub fn match_all(
        &self,
        subject: &[u8],
        start_offset: usize,
        global: bool,
        first_opts: u32,
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
            let len = subject.len();
            let read = |md| {
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
                (spans, mark)
            };
            // PHP's global-scan loop (php_pcre_match_impl): a match is
            // recorded, then the scan resumes at offsets[1] — the match
            // END. An empty match additionally retries anchored+notempty
            // at the same point, and THAT retry match is recorded too
            // (`goto matched`); only a NOMATCH retry bumps past one
            // character (bug70232: dropping the retry match loses hits
            // like `\K`-adjacent alternations).
            let mut start_offset2 = start_offset;
            let mut count = pcre2_match_8(
                self.code,
                subject.as_ptr(),
                len,
                start_offset2,
                first_opts,
                md,
                mctx,
            );
            'scan: loop {
                if count < 0 {
                    if count == PCRE2_ERROR_NOMATCH {
                        break;
                    }
                    err = count;
                    break;
                }
                // `matched:` — the empty-match retry lands here with a
                // fresh count that must be recorded, not re-validated.
                loop {
                    let (spans, mark) = read(md);
                    out.push(PcreMatch { spans, mark });
                    if !global {
                        break 'scan;
                    }
                    let (m0, m1) = {
                        let ov = pcre2_get_ovector_pointer_8(md);
                        (*ov, *ov.add(1))
                    };
                    start_offset2 = m1;
                    if start_offset2 == m0 {
                        // Empty match — retry anchored+notempty at the
                        // same position.
                        count = pcre2_match_8(
                            self.code,
                            subject.as_ptr(),
                            len,
                            start_offset2,
                            PCRE2_NO_UTF_CHECK | PCRE2_NOTEMPTY_ATSTART | PCRE2_ANCHORED,
                            md,
                            mctx,
                        );
                        if count >= 0 {
                            // Record the retry match (goto matched).
                            continue;
                        }
                        if count == PCRE2_ERROR_NOMATCH {
                            if start_offset2 >= len {
                                break 'scan;
                            }
                            // Bump one character — UTF-8 aware under /u.
                            start_offset2 += if self.utf8 {
                                let mut n = 1usize;
                                while start_offset2 + n < len
                                    && (subject[start_offset2 + n] & 0xC0) == 0x80
                                {
                                    n += 1;
                                }
                                n
                            } else {
                                1
                            };
                        } else {
                            err = count;
                            break 'scan;
                        }
                    }
                    count = pcre2_match_8(
                        self.code,
                        subject.as_ptr(),
                        len,
                        start_offset2,
                        PCRE2_NO_UTF_CHECK,
                        md,
                        mctx,
                    );
                    continue 'scan;
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
pub fn compile(src: &[u8], options: u32, extra_options: u32) -> Result<PcreRe, String> {
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
            // PHP maps the \C rejection to its own wording
            // (php_pcre.c): it is always a /u-incompatibility error.
            if errcode as u32 == PCRE2_ERROR_BACKSLASH_C_CALLER_DISABLED {
                return Err(format!(
                    "using \\C is incompatible with the 'u' modifier at offset {}",
                    erroff
                ));
            }
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
