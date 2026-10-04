// temporary probe — not committed
use pcre2_sys::*;
use std::ptr;
fn main() {
    unsafe {
        let mut err: std::os::raw::c_int = 0;
        let mut off: usize = 0;
        let pat = b"\\b";
        let code = pcre2_compile_8(pat.as_ptr(), pat.len(), PCRE2_UTF | PCRE2_MATCH_INVALID_UTF, &mut err, &mut off, ptr::null_mut());
        assert!(!code.is_null());
        for (subj, so) in [
            ("\u{2019}".as_bytes().to_vec(), 0usize), ("\u{2019}".as_bytes().to_vec(), 1), ("\u{2019}".as_bytes().to_vec(), 3),
            (b"VA\xffLID".to_vec(), 0), (b"VA\xffLID".to_vec(), 3), (b"VA\xffLID".to_vec(), 4),
        ] {
            let b: &[u8] = &subj;
            let md = pcre2_match_data_create_from_pattern_8(code, ptr::null_mut());
            let rc = pcre2_match_8(code, b.as_ptr(), b.len(), so, 0, md, ptr::null_mut());
            let ov = pcre2_get_ovector_pointer_8(md);
            let (s,e) = if rc > 0 { (*ov, *ov.add(1)) } else { (usize::MAX, usize::MAX) };
            println!("subj={:?} off={} rc={} span=({},{})", String::from_utf8_lossy(b), so, rc, s, e);
            pcre2_match_data_free_8(md);
        }
        let pat2 = b"(.*)\\C";
        let c2 = pcre2_compile_8(pat2.as_ptr(), pat2.len(), PCRE2_UTF | PCRE2_MATCH_INVALID_UTF, &mut err, &mut off, ptr::null_mut());
        let mut buf=[0u8;128]; let n=pcre2_get_error_message_8(err, buf.as_mut_ptr().cast(), 128);
        println!("\\C+u: code_null={} err={} off={} msg={}", c2.is_null(), err, off, String::from_utf8_lossy(&buf[..n.max(0)as usize]));
        for p in ["(*NO_JIT)^[\\x{0100}-\\x{017f}]{1,63}$", "(*UTF)(*NO_JIT)^a$"] {
            let c = pcre2_compile_8(p.as_ptr(), p.len(), PCRE2_UTF|PCRE2_UCP|PCRE2_MATCH_INVALID_UTF, &mut err, &mut off, ptr::null_mut());
            let n=pcre2_get_error_message_8(err, buf.as_mut_ptr().cast(), 128);
            println!("verb {:?}: ok={} err={} off={} msg={}", p, !c.is_null(), err, off, String::from_utf8_lossy(&buf[..n.max(0)as usize]));
        }
        let c = pcre2_compile_8(b"+".as_ptr(), 1, 0, &mut err, &mut off, ptr::null_mut());
        let n=pcre2_get_error_message_8(err, buf.as_mut_ptr().cast(), 128);
        println!("'+': ok={} off={} msg={}", !c.is_null(), off, String::from_utf8_lossy(&buf[..n.max(0)as usize]));
        let mut jit: i32 = -1;
        pcre2_config_8(PCRE2_CONFIG_JIT, (&mut jit as *mut i32).cast());
        println!("PCRE2_CONFIG_JIT = {}", jit);
    }
}
