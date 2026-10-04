//! semver-lite — the composer constraint subset phpun needs for
//! dependency resolution. Supported: `*`, exact `1.2.3`, partial
//! (`1.2` = `1.2.*`, `1` = `1.*`), wildcards, caret (`^1.2`), tilde
//! (`~1.2`), comparison ops (`>=` `<=` `>` `<` `=` `!=`), hyphen ranges
//! (`1.0 - 2.0`), `||`/`|` alternation, space/comma conjunction, and
//! `@stability` suffixes. NOT supported: `dev-*` branch constraints and
//! `version:constraint` aliasing (error, not silent wrong answer).

use std::cmp::Ordering;

/// Stability rank — composer order: dev < alpha < beta < RC < stable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Stab {
    Dev = 0,
    Alpha = 1,
    Beta = 2,
    RC = 3,
    Stable = 4,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ver {
    /// Composer's version_normalized is always 4 components.
    pub nums: [u64; 4],
    pub stab: Stab,
    pub stab_num: u64,
}

impl Ver {
    pub fn parse(s: &str) -> Option<Ver> {
        let mut s = s.trim();
        s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);
        // Split stability suffix at first '-', '_' or '.'-separated
        // keyword: 1.2.3-beta2, 1.2.3.0-RC1, 1.2.3.4.5-patch?
        let (nums_part, suffix) = match s.find(['-', '_']) {
            Some(i) => (&s[..i], Some(&s[i + 1..])),
            None => (s, None),
        };
        // "1.2.3.pl1" style also surfaces as ".patchN"/".betaN" — handle
        // a trailing ".<stab><n>" component when the numeric part is
        // otherwise complete (composer normalizes those to -stabN).
        let mut segs: Vec<u64> = Vec::new();
        let mut suffix = suffix.map(|x| x.to_string());
        for seg in nums_part.split('.') {
            if let Ok(n) = seg.parse::<u64>() {
                segs.push(n);
            } else if suffix.is_none() && !seg.is_empty() && segs.len() >= 3 {
                suffix = Some(seg.to_string());
                break;
            } else {
                return None;
            }
        }
        if segs.is_empty() || segs.len() > 4 {
            return None;
        }
        while segs.len() < 4 {
            segs.push(0);
        }
        let (stab, stab_num) = parse_stab(suffix.as_deref())?;
        Some(Ver {
            nums: [segs[0], segs[1], segs[2], segs[3]],
            stab,
            stab_num,
        })
    }

    pub fn cmp_ver(&self, o: &Ver) -> Ordering {
        self.nums
            .cmp(&o.nums)
            .then(self.stab.cmp(&o.stab))
            .then(self.stab_num.cmp(&o.stab_num))
    }

    pub fn display(&self) -> String {
        let mut s = self.nums[..3]
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(".");
        if self.nums[3] != 0 {
            s.push_str(&format!(".{}", self.nums[3]));
        }
        match self.stab {
            Stab::Stable => {}
            Stab::Dev => s.push_str(&format!("-dev{}", self.stab_num)),
            Stab::Alpha => s.push_str(&format!("-alpha{}", self.stab_num)),
            Stab::Beta => s.push_str(&format!("-beta{}", self.stab_num)),
            Stab::RC => s.push_str(&format!("-RC{}", self.stab_num)),
        }
        s
    }
}

fn parse_stab(s: Option<&str>) -> Option<(Stab, u64)> {
    let s = match s {
        None | Some("") => return Some((Stab::Stable, 0)),
        Some(s) => s.to_lowercase(),
    };
    // peel optional leading "patch"/"pl" etc are not stabilities — bail
    let (name, num) = {
        let i = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        (&s[..i], &s[i..])
    };
    let stab = match name {
        "dev" | "patch" | "pl" | "p" => Stab::Dev,
        "alpha" | "a" => Stab::Alpha,
        "beta" | "b" | "bata" => Stab::Beta,
        "rc" => Stab::RC,
        "stable" => Stab::Stable,
        _ => return None,
    };
    Some((stab, num.parse::<u64>().unwrap_or(0)))
}

/// Minimum stability the resolver accepts (root composer.json's
/// `minimum-stability`, default Stable).
pub fn min_stab(name: &str) -> Stab {
    match name {
        "dev" => Stab::Dev,
        "alpha" => Stab::Alpha,
        "beta" => Stab::Beta,
        "RC" | "rc" => Stab::RC,
        _ => Stab::Stable,
    }
}

/// One conjunction term's numeric bound.
pub enum Bound {
    Ge(Ver),
    Gt(Ver),
    Le(Ver),
    Lt(Ver),
    Eq(Ver),
    Ne(Ver),
}

/// `^a.b.c`: >=a.b.c and <next-breaking. Composer caret: 0.y.z pins the
/// first non-zero component.
fn caret_bound(v: &[u64]) -> [u64; 4] {
    let mut hi = [v[0], v[1], v[2], *v.get(3).unwrap_or(&0)];
    for i in 0..3 {
        if hi[i] != 0 {
            hi[i] += 1;
            for x in hi.iter_mut().skip(i + 1) {
                *x = 0;
            }
            return hi;
        }
    }
    // ^0.0.0 — composer: >=0.0.0 <0.1.0
    hi[1] += 1;
    hi
}

/// `~a.b`: >=a.b <a+1.0; `~a.b.c`: >=a.b.c <a.(b+1).0; `~a`: <a+1.0.
/// Composer: tilde bumps the SECOND-to-last specified component's
/// parent — ~1.2 → <2.0, ~1.2.3 → <1.3.0, ~1.2.3.4 → <1.2.4.
fn tilde_bound(parts: usize, v: &[u64]) -> [u64; 4] {
    let mut hi = [v[0], v[1], v[2], *v.get(3).unwrap_or(&0)];
    let bump = parts.saturating_sub(2).min(2);
    hi[bump] += 1;
    for x in hi.iter_mut().skip(bump + 1) {
        *x = 0;
    }
    hi
}

fn ver4(nums: [u64; 4]) -> Ver {
    Ver {
        nums,
        stab: Stab::Stable,
        stab_num: 0,
    }
}

fn parse_term(t: &str) -> Result<Vec<Bound>, String> {
    let t = t.trim();
    if t.is_empty() || t == "*" || t.eq_ignore_ascii_case("x") {
        return Ok(vec![]);
    }
    // Hyphen range "a - b" (may arrive pre-split on spaces — handle
    // the "a-b" compact form only when both sides parse).
    for op in [">=", "<=", "!=", "==", ">", "<", "="] {
        if let Some(rest) = t.strip_prefix(op) {
            let v = Ver::parse(rest.trim())
                .ok_or_else(|| format!("bad version in constraint '{}'", t))?;
            let b = match op {
                ">=" => Bound::Ge(v),
                "<=" => Bound::Le(v),
                "!=" => Bound::Ne(v),
                ">" => Bound::Gt(v),
                _ => Bound::Eq(v),
            };
            return Ok(vec![b]);
        }
    }
    if let Some(rest) = t.strip_prefix('^') {
        // split into components to know how many were given
        let body = rest.trim();
        if body.contains('*') || body.eq_ignore_ascii_case("x") {
            // ^1.x == ~1.x? composer: ^1.x = >=1.0 <2.0
            let pfx = body.trim_end_matches(['*', 'x', 'X', '.']);
            let have = pfx.split('.').filter(|s| !s.is_empty()).count();
            if have == 0 {
                return Ok(vec![]);
            }
            let v = Ver::parse(body.replace(['*', 'x', 'X'], "0").as_str())
                .ok_or_else(|| format!("bad version in constraint '{}'", t))?;
            let mut hi = [0u64; 4];
            hi[..have].copy_from_slice(&v.nums[..have]);
            if have < 4 {
                hi[have - 1] += 1;
            }
            return Ok(vec![Bound::Ge(v), Bound::Lt(ver4(hi))]);
        }
        let v = Ver::parse(body).ok_or_else(|| format!("bad version in constraint '{}'", t))?;
        return Ok(vec![
            Bound::Ge(v.clone()),
            Bound::Lt(ver4(caret_bound(&v.nums))),
        ]);
    }
    if let Some(rest) = t.strip_prefix('~') {
        let body = rest.trim();
        let have = body.split('.').count();
        let v = Ver::parse(body).ok_or_else(|| format!("bad version in constraint '{}'", t))?;
        return Ok(vec![
            Bound::Ge(v.clone()),
            Bound::Lt(ver4(tilde_bound(have, &v.nums))),
        ]);
    }
    if t.contains('*') || t.to_lowercase().contains('x') {
        // wildcard: "1.2.*" / "1.*" — >= a.b.0 < a.(b+1).0
        let keep: String = t
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let parts: Vec<u64> = keep
            .split('.')
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u64>())
            .collect::<Result<_, _>>()
            .map_err(|_| format!("bad wildcard constraint '{}'", t))?;
        if parts.is_empty() {
            return Ok(vec![]);
        }
        let mut lo = [0u64; 4];
        let mut hi = [0u64; 4];
        for (i, p) in parts.iter().enumerate() {
            lo[i] = *p;
            hi[i] = *p;
        }
        hi[parts.len() - 1] += 1;
        return Ok(vec![Bound::Ge(ver4(lo)), Bound::Lt(ver4(hi))]);
    }
    // bare version: exact if 3+ parts, wildcard prefix if fewer
    let v = Ver::parse(t).ok_or_else(|| format!("bad constraint '{}'", t))?;
    let have = t
        .trim_start_matches('v')
        .split('.')
        .take_while(|s| s.chars().all(|c| c.is_ascii_digit()))
        .count();
    if have >= 3 {
        Ok(vec![Bound::Eq(v)])
    } else {
        // "1.2" => >=1.2.0 <1.3.0;  "1" => >=1.0.0 <2.0.0
        let mut lo = [0u64; 4];
        let mut hi = [0u64; 4];
        lo[..have].copy_from_slice(&v.nums[..have]);
        hi[..have].copy_from_slice(&v.nums[..have]);
        hi[have - 1] += 1;
        Ok(vec![Bound::Ge(ver4(lo)), Bound::Lt(ver4(hi))])
    }
}

/// Full constraint string → Vec of alternatives, each a Vec of bounds.
/// Errors on unsupported forms (dev-*, branch aliases).
pub fn parse_constraint(c: &str) -> Result<Vec<Vec<Bound>>, String> {
    let c = c.trim();
    // strip @stability
    let c = c.split('@').next().unwrap_or(c).trim();
    if c.starts_with("dev-") || c.starts_with("dev_") {
        return Err(format!("branch constraint '{}' not supported", c));
    }
    let mut alts = Vec::new();
    for alt in c.split("||").flat_map(|a| a.split('|')) {
        // hyphen ranges contain " - " — expand before space-splitting
        let alt = alt.trim();
        let alt = expand_hyphen(alt);
        let mut bounds = Vec::new();
        for term in alt.split([' ', ',']) {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            bounds.extend(parse_term(term)?);
        }
        alts.push(bounds);
    }
    Ok(alts)
}

/// `1.0 - 2.0` → `>=1.0.0 <3.0.0`? Composer hyphen range: lower bound
/// inclusive; upper `2.0` partial → `<3.0` (wildcard-inclusive), full
/// `2.0.0` → `<=2.0.0`.
fn expand_hyphen(alt: &str) -> String {
    let mut out = String::new();
    let mut rest = alt;
    while let Some(i) = rest.find(" - ") {
        let left = rest[..i].trim_end();
        let right_start = i + 3;
        let right: String = rest[right_start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ',')
            .collect();
        let tail = &rest[right_start + right.len()..];
        // the left endpoint is the last whitespace/comma-separated token
        let (head, lo) = match left.rfind([' ', ',']) {
            Some(j) => (&left[..=j], left[j + 1..].trim()),
            None => ("", left),
        };
        out.push_str(head);
        if let (Ok(lv), Ok(rv)) = (
            Ver::parse(lo).ok_or(()),
            Ver::parse(right.as_str()).ok_or(()),
        ) {
            let rparts = right.split('.').count();
            out.push_str(&format!(">={}", lv.display()));
            if rparts >= 3 {
                out.push_str(&format!(",<={}", rv.display()));
            } else {
                let mut hi = rv.nums;
                hi[rparts.max(1) - 1] += 1;
                out.push_str(&format!(",<{}", ver4(hi).display()));
            }
        } else {
            // not a range — keep the text
            out.push_str(lo);
            out.push_str(" - ");
            out.push_str(&right);
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Does `v` satisfy constraint `c`?
pub fn satisfies(v: &Ver, c: &str) -> bool {
    let Ok(alts) = parse_constraint(c) else {
        return false;
    };
    alts.iter().any(|bounds| {
        bounds.iter().all(|b| match b {
            Bound::Ge(o) => v.cmp_ver(o) != Ordering::Less,
            Bound::Gt(o) => v.cmp_ver(o) == Ordering::Greater,
            Bound::Le(o) => v.cmp_ver(o) != Ordering::Greater,
            Bound::Lt(o) => v.cmp_ver(o) == Ordering::Less,
            Bound::Eq(o) => v.cmp_ver(o) == Ordering::Equal,
            Bound::Ne(o) => v.cmp_ver(o) != Ordering::Equal,
        })
    })
}

/// Constraint mentions an explicit stability suffix or dev branch?
pub fn wants_dev(c: &str) -> Option<Stab> {
    let c = c.trim();
    if c.starts_with("dev-") || c.contains("@dev") {
        return Some(Stab::Dev);
    }
    for (needle, s) in [
        ("@alpha", Stab::Alpha),
        ("@beta", Stab::Beta),
        ("@RC", Stab::RC),
        ("@rc", Stab::RC),
        ("-dev", Stab::Dev),
        ("-alpha", Stab::Alpha),
        ("-beta", Stab::Beta),
        ("-RC", Stab::RC),
        ("-rc", Stab::RC),
    ] {
        if c.contains(needle) {
            return Some(s);
        }
    }
    None
}
