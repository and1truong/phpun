//! Hash/codec builtins: md5/sha1/crc32/hash, base64, openssl_*, random_bytes.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        // ----- hashing -----
        "md5" => Value::str(md5_hex(&arg_bs(it, args, 0))),
        "sha1" => Value::str(sha1_hex(&arg_bs(it, args, 0))),
        "crc32" => Value::Int(crc32(&arg_bs(it, args, 0)) as i64),
        "hash" => {
            let algo = arg_str(it, args, 0).to_lowercase();
            let data = arg_str(it, args, 1);
            match algo.as_str() {
                "md5" => Value::str(md5_hex(data.as_bytes())),
                "sha1" => Value::str(sha1_hex(data.as_bytes())),
                "crc32" | "crc32b" => Value::str(format!("{:08x}", crc32(data.as_bytes()))),
                _ => Value::Bool(false),
            }
        }
        "hash_equals" => Value::Bool(arg_str(it, args, 0) == arg_str(it, args, 1)),
        "hash_pbkdf2" => {
            let algo = arg_str(it, args, 0).to_lowercase();
            let pass = arg_bs(it, args, 1);
            let salt = arg_bs(it, args, 2);
            let iters = arg(args, 3).to_int();
            let length = args.get(4).map(|c| c.borrow().to_int()).unwrap_or(0);
            let raw = args.get(5).map(|c| c.borrow().is_truthy()).unwrap_or(false);
            let digest_len = match algo.as_str() {
                "md5" => 16,
                "sha1" => 20,
                "sha256" => 32,
                "sha384" => 48,
                "sha512" => 64,
                _ => {
                    return err(
                        "ValueError",
                        "hash_pbkdf2(): Argument #1 ($algo) must be a valid cryptographic hashing algorithm",
                    )
                }
            };
            if iters <= 0 {
                return err(
                    "ValueError",
                    "hash_pbkdf2(): Argument #4 ($iterations) must be greater than 0",
                );
            }
            // zend: length is the OUTPUT-string length (hex chars
            // unless raw); length<=0 → one full digest.
            let out_bytes = if length > 0 {
                ((length + 1) / 2) as usize
            } else {
                digest_len as usize
            };
            let dk = pbkdf2(&algo, &pass, &salt, iters, out_bytes);
            if raw {
                Value::bytes(dk)
            } else {
                let hex = dk.iter().map(|b| format!("{:02x}", b)).collect::<String>();
                Value::str(if length > 0 {
                    hex.chars().take(length as usize).collect()
                } else {
                    hex
                })
            }
        }
        "crc32_combine" => Value::Int(0),

        // ----- encoding -----
        "base64_encode" => Value::str(base64_encode(&arg_bs(it, args, 0))),
        "base64_decode" => match base64_decode(&arg_str(it, args, 0)) {
            Some(b) => Value::bytes(b),
            None => Value::Bool(false),
        },
        "openssl_x509_parse" => {
            // Not a real X.509 parser: returns a plausible array when the
            // PEM input holds a certificate block — enough for offline
            // CA-file validation (composer/ca-bundle checks truthiness).
            let pem = arg_str(it, args, 0);
            if pem.contains("BEGIN CERTIFICATE") {
                let mut a = PhpArray::new();
                let mut subj = PhpArray::new();
                subj.set(ArrKey::Str("CN".into()), Value::str(""));
                a.set(
                    ArrKey::Str("subject".into()),
                    Value::Array(Rc::new(RefCell::new(subj))),
                );
                a.set(ArrKey::Str("validFrom_time_t".into()), Value::Int(0));
                a.set(ArrKey::Str("validTo_time_t".into()), Value::Int(i64::MAX));
                Value::Array(Rc::new(RefCell::new(a)))
            } else {
                Value::Bool(false)
            }
        }
        "openssl_random_pseudo_bytes" | "random_bytes" => {
            let n = arg(args, 0).to_int().max(0) as usize;
            let mut b = vec![0u8; n];
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9e3779b97f4a7c15);
            let mut x = seed;
            for byte in b.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *byte = x as u8;
            }
            Value::bytes(b)
        }
        _ => return Ok(None),
    }))
}

// ----- helpers -----

fn md5_hex(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn crc32(data: &[u8]) -> u32 {
    // IEEE CRC32
    let mut crc = 0xFFFFFFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub(in crate::builtins) fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let v = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(v >> 18) as usize & 63] as char);
        out.push(T[(v >> 12) as usize & 63] as char);
        out.push(if n > 1 {
            T[(v >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if n > 2 {
            T[v as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub(in crate::builtins) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// PBKDF2-HMAC. `algo` keys into the registered digests; unknown
/// algos return None (zend emits its algo-validation warning first).
fn pbkdf2(algo: &str, pass: &[u8], salt: &[u8], iters: i64, need: usize) -> Vec<u8> {
    fn hmac<F>(key: &[u8], block: usize, mut h: F, msg: &[u8]) -> Vec<u8>
    where
        F: FnMut(&[u8]) -> Vec<u8>,
    {
        let mut k = if key.len() > block {
            h(key)
        } else {
            key.to_vec()
        };
        k.resize(block, 0);
        let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
        let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
        let mut inner = ipad;
        inner.extend_from_slice(msg);
        let ih = h(&inner);
        let mut outer = opad;
        outer.extend_from_slice(&ih);
        h(&outer)
    }
    use sha1::Digest as _;
    type DigestFn = Box<dyn Fn(&[u8]) -> Vec<u8>>;
    let (h, block): (DigestFn, usize) = match algo {
        "md5" => (Box::new(|m| md5::compute(m).0.to_vec()), 64),
        "sha1" => (Box::new(|m| sha1::Sha1::digest(m).to_vec()), 64),
        "sha256" => (Box::new(|m| sha2::Sha256::digest(m).to_vec()), 64),
        "sha384" => (Box::new(|m| sha2::Sha384::digest(m).to_vec()), 128),
        "sha512" => (Box::new(|m| sha2::Sha512::digest(m).to_vec()), 128),
        _ => unreachable!(),
    };
    // T_i = U1 ^ U2 ^ ... ^ Uiters for each block counter i.
    let mut out = Vec::new();
    let mut i = 1u32;
    while out.len() < need {
        let mut msg = salt.to_vec();
        msg.extend_from_slice(&i.to_be_bytes());
        let mut u = hmac(pass, block, &*h, &msg);
        let mut t = u.clone();
        for _ in 1..iters {
            u = hmac(pass, block, &*h, &u);
            for (x, y) in t.iter_mut().zip(&u) {
                *x ^= y;
            }
        }
        out.extend_from_slice(&t);
        i += 1;
    }
    out.truncate(need);
    out
}
