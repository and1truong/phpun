use pcre2_sys::*;
fn main() {
    unsafe {
        let mut err: std::os::raw::c_int = 0;
        let mut off: usize = 0;
        let code = pcre2_compile_8(b"^[a-z]+".as_ptr(), 7, 0, &mut err, &mut off, std::ptr::null_mut());
        let md = pcre2_match_data_create_from_pattern_8(code, std::ptr::null_mut());
        for lim in [0u32, 1, 2, 3] {
            let mctx = pcre2_match_context_create_8(std::ptr::null_mut());
            pcre2_set_depth_limit_8(mctx, lim);
            let rc = pcre2_match_8(code, b"a".as_ptr(), 1, 0, 0, md, mctx);
            println!("depth_limit={} rc={}", lim, rc);
            pcre2_match_context_free_8(mctx);
        }
        // nested pattern needs more depth
        let code2 = pcre2_compile_8(b"^(([a-z]+)x)+$".as_ptr(), 12, 0, &mut err, &mut off, std::ptr::null_mut());
        for lim in [1u32, 2, 3, 10] {
            let mctx = pcre2_match_context_create_8(std::ptr::null_mut());
            pcre2_set_depth_limit_8(mctx, lim);
            let rc = pcre2_match_8(code2, b"axax".as_ptr(), 4, 0, 0, md, mctx);
            println!("nested depth_limit={} rc={}", lim, rc);
            pcre2_match_context_free_8(mctx);
        }
    }
}
