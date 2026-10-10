//! Date/time builtins: date/mktime/strtotime and civil-day math.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "time" | "time_sleep_until" => Value::Int(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        ),
        "microtime" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            if arg(args, 0).is_truthy() {
                Value::Float(now.as_secs() as f64 + now.subsec_micros() as f64 / 1e6)
            } else {
                Value::str(format!(
                    "{:.8} {}",
                    now.subsec_micros() as f64 / 1e6,
                    now.as_secs()
                ))
            }
        }
        "hrtime" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            // PHP: hrtime() -> [secs, nanos]; hrtime(true) -> int nanos.
            if arg(args, 0).is_truthy() {
                Value::Int(now.as_nanos() as i64)
            } else {
                let mut a = PhpArray::new();
                a.push(Value::Int(now.as_secs() as i64));
                a.push(Value::Int(now.subsec_nanos() as i64));
                Value::Array(Rc::new(RefCell::new(a)))
            }
        }
        "date_default_timezone_set" | "date_default_timezone_get" => {
            if name.ends_with("set") {
                Value::Bool(true)
            } else {
                Value::str("UTC")
            }
        }
        "date" | "gmdate" => {
            let fmt = arg_str(it, args, 0);
            let ts = if args.len() > 1 {
                arg(args, 1).to_int()
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            };
            Value::str(date_format(&fmt, ts))
        }
        "mktime" | "gmmktime" => {
            let h = arg(args, 0).to_int();
            let m = arg(args, 1).to_int();
            let s = arg(args, 2).to_int();
            let mo = if args.len() > 3 {
                arg(args, 3).to_int()
            } else {
                1
            };
            let d = if args.len() > 4 {
                arg(args, 4).to_int()
            } else {
                1
            };
            let y = if args.len() > 5 {
                arg(args, 5).to_int()
            } else {
                1970
            };
            Value::Int(days_from_civil(y, mo, d) * 86400 + h * 3600 + m * 60 + s)
        }
        "checkdate" => {
            let (m, d, y) = (
                arg(args, 0).to_int(),
                arg(args, 1).to_int(),
                arg(args, 2).to_int(),
            );
            Value::Bool((1..=12).contains(&m) && (1..=31).contains(&d) && (1..=32767).contains(&y))
        }
        "strtotime" => {
            let s = arg_str(it, args, 0);
            let base = if args.len() > 1 && !matches!(arg(args, 1), Value::Null) {
                arg(args, 1).to_int()
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            };
            match strtotime_parse(&s, base) {
                Some(ts) => Value::Int(ts),
                None => Value::Bool(false),
            }
        }
        "date_parse" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "microtime_float" => Value::Float(0.0),
        "date_sunrise" | "date_sunset" | "date_sun_info" => Value::Bool(false),
        _ => return Ok(None),
    }))
}

// ----- helpers -----

pub(crate) fn date_format(fmt: &str, ts: i64) -> String {
    date_format_tz(fmt, ts, 0, "UTC", "UTC")
}

/// `ts` here is the WALL seconds (instant + off). `off` feeds O/P/Z,
/// `e_name`/`t_name` the e/T letters (zone name vs abbreviation —
/// for numeric-offset zones both are the `±HH:MM` string).
pub(crate) fn date_format_tz(fmt: &str, ts: i64, off: i64, e_name: &str, t_name: &str) -> String {
    // minimal strftime-ish for the common tokens
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let h = secs / 3600;
    let mi = (secs % 3600) / 60;
    let s = secs % 60;
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
            continue;
        }
        match c {
            'Y' => out.push_str(&format!("{:04}", y)),
            'y' => out.push_str(&format!("{:02}", y % 100)),
            'm' => out.push_str(&format!("{:02}", m)),
            'n' => out.push_str(&format!("{}", m)),
            'd' => out.push_str(&format!("{:02}", d)),
            'j' => out.push_str(&format!("{}", d)),
            'H' => out.push_str(&format!("{:02}", h)),
            'G' => out.push_str(&format!("{}", h)),
            'h' => out.push_str(&format!("{:02}", if h % 12 == 0 { 12 } else { h % 12 })),
            'g' => out.push_str(&format!("{}", if h % 12 == 0 { 12 } else { h % 12 })),
            'i' => out.push_str(&format!("{:02}", mi)),
            's' => out.push_str(&format!("{:02}", s)),
            'a' => out.push_str(if h < 12 { "am" } else { "pm" }),
            'A' => out.push_str(if h < 12 { "AM" } else { "PM" }),
            'D' => out.push_str(
                ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                    [(days + 4).rem_euclid(7) as usize],
            ),
            'l' => out.push_str(
                [
                    "Sunday",
                    "Monday",
                    "Tuesday",
                    "Wednesday",
                    "Thursday",
                    "Friday",
                    "Saturday",
                ][(days + 4).rem_euclid(7) as usize],
            ),
            'M' => out.push_str(
                [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ][(m - 1) as usize],
            ),
            'F' => out.push_str(
                [
                    "January",
                    "February",
                    "March",
                    "April",
                    "May",
                    "June",
                    "July",
                    "August",
                    "September",
                    "October",
                    "November",
                    "December",
                ][(m - 1) as usize],
            ),
            'U' => out.push_str(&ts.to_string()),
            'e' => out.push_str(e_name),
            'T' => out.push_str(t_name),
            'O' => out.push_str(&format!(
                "{}{:02}{:02}",
                if off < 0 { '-' } else { '+' },
                off.abs() / 3600,
                off.abs() % 3600 / 60
            )),
            'P' => out.push_str(&format!(
                "{}{:02}:{:02}",
                if off < 0 { '-' } else { '+' },
                off.abs() / 3600,
                off.abs() % 3600 / 60
            )),
            'Z' => out.push_str(&off.to_string()),
            'c' => out.push_str(&format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
                y,
                m,
                d,
                h,
                mi,
                s,
                if off < 0 { '-' } else { '+' },
                off.abs() / 3600,
                off.abs() % 3600 / 60
            )),
            'r' => out.push_str(&format!(
                "{}, {:02} {} {:04} {:02}:{:02}:{:02} {}{:02}{:02}",
                ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                    [(days + 4).rem_euclid(7) as usize],
                d,
                [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec"
                ][(m - 1) as usize],
                y,
                h,
                mi,
                s,
                if off < 0 { '-' } else { '+' },
                off.abs() / 3600,
                off.abs() % 3600 / 60
            )),
            'w' => out.push_str(&(days + 4).rem_euclid(7).to_string()),
            'z' => {
                let jan1 = days_from_civil(y, 1, 1);
                out.push_str(&(days - jan1).to_string());
            }
            't' => {
                let next = days_from_civil(
                    if m == 12 { y + 1 } else { y },
                    if m == 12 { 1 } else { m + 1 },
                    1,
                );
                let cur = days_from_civil(y, m, 1);
                out.push_str(&(next - cur).to_string());
            }
            'L' => out.push_str(if is_leap(y) { "1" } else { "0" }),
            'S' => out.push_str(match d % 10 {
                1 if d != 11 => "st",
                2 if d != 12 => "nd",
                3 if d != 13 => "rd",
                _ => "th",
            }),
            'N' => out.push_str(&((days + 3).rem_euclid(7) + 1).to_string()),
            'W' => {
                let jan1 = days_from_civil(y, 1, 1);
                let week = (days - jan1) / 7 + 1;
                out.push_str(&format!("{:02}", week));
            }
            c => out.push(c),
        }
    }
    out
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Howard Hinnant's civil calendar math.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------- strtotime ----------

#[derive(Clone)]
enum STok {
    I(i64, usize), // value, digit count
    W(String),     // lowercased alpha word
    C(u8),         // punctuation byte
}

fn strto_toks(b: &[u8]) -> Vec<STok> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n: i64 = std::str::from_utf8(&b[s..i])
                .unwrap_or("")
                .parse()
                .unwrap_or(0);
            v.push(STok::I(n, i - s));
        } else if c.is_ascii_alphabetic() {
            let s = i;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            v.push(STok::W(String::from_utf8_lossy(&b[s..i]).to_lowercase()));
        } else if c >= 0x80 {
            i += 1; // skip multibyte bytes
        } else {
            v.push(STok::C(c));
            i += 1;
        }
    }
    v
}

/// Date/time under construction: epoch-day + seconds-in-day.
#[derive(Clone, Copy)]
struct Dt {
    days: i64,
    secs: i64,
}

impl Dt {
    fn from_ts(ts: i64) -> Dt {
        Dt {
            days: ts.div_euclid(86400),
            secs: ts.rem_euclid(86400),
        }
    }
    fn ymd(&self) -> (i64, i64, i64) {
        civil_from_days(self.days)
    }
    fn set_ymd(&mut self, y: i64, m: i64, d: i64) {
        self.days = days_from_civil(y, m, d);
    }
    /// 0 = Sunday .. 6 = Saturday.
    fn dow(&self) -> i64 {
        (self.days + 4).rem_euclid(7)
    }
    /// 0 = Monday .. 6 = Sunday.
    fn iso_dow(&self) -> i64 {
        (self.days + 3).rem_euclid(7)
    }
    fn add_secs(&mut self, n: i64) {
        let t = self.secs + n;
        self.days += t.div_euclid(86400);
        self.secs = t.rem_euclid(86400);
    }
    fn add_months(&mut self, n: i64) {
        let (y, m, d) = self.ymd();
        let t = y * 12 + (m - 1) + n;
        self.set_ymd(t.div_euclid(12), t.rem_euclid(12) + 1, d);
    }
    /// Business-day step (skip Sat/Sun).
    fn add_weekdays(&mut self, n: i64) {
        let step = |d: &mut Dt, fwd: bool| loop {
            d.days += if fwd { 1 } else { -1 };
            let dow = d.dow();
            if (1..=5).contains(&dow) {
                break;
            }
        };
        for _ in 0..n.abs() {
            step(self, n > 0);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SlotBeh {
    Bare, // bare/"this": forward-circular within the week (today matches)
    Next, // strict next (today's match skipped)
    Last, // strict previous
    Ago,  // "X ago": two strict-back seeks
}

#[derive(Clone, Copy)]
enum PostOp {
    SeekDow(i64, i64), // N strict dow seeks (sign = direction)
    Biz(i64),          // N business-day steps
    WeekBound(i64),    // +1 next / 0 this / -1 last ISO week (Monday anchor)
}

#[derive(Clone, Copy)]
enum Unit {
    Sub, // sub-second units: no whole-second effect
    Sec,
    Min,
    Hour,
    Day,
    Week,
    Fortnight,
    Weekday,
    Month,
    Year,
}

/// Relative-delta accumulators (Zend timelib's relative fields): each unit
/// accumulates additively; `tomorrow`/`yesterday` SET the day count and flag
/// a time reset that applies before the deltas are added.
#[derive(Default, Clone, Copy)]
struct Bag {
    months: i64,
    days: i64,
    biz: i64,
    secs: i64,
    reset: bool,
}

impl Bag {
    fn add(&mut self, n: i64, u: Unit) {
        match u {
            Unit::Sub => {}
            Unit::Sec => self.secs += n,
            Unit::Min => self.secs += n * 60,
            Unit::Hour => self.secs += n * 3600,
            Unit::Day => self.days += n,
            Unit::Week => self.days += n * 7,
            Unit::Fortnight => self.days += n * 14,
            Unit::Weekday => self.biz += n,
            Unit::Month => self.months += n,
            Unit::Year => self.months += n * 12,
        }
    }
    fn set_day(&mut self, n: i64) {
        self.days = n;
        self.reset = true;
    }
    fn negate(&mut self) {
        self.months = -self.months;
        self.days = -self.days;
        self.biz = -self.biz;
        self.secs = -self.secs;
    }
}

struct StrtoP {
    dt: Dt,
    tz_off: Option<i64>,
    slot: Option<(i64, SlotBeh)>,
    post: Vec<PostOp>,
    bag: Bag,
    i: usize,
}

fn strto_month(w: &str) -> Option<i64> {
    Some(match w {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

fn strto_dow(w: &str) -> Option<i64> {
    Some(match w {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

fn strto_ord(w: &str) -> Option<i64> {
    Some(match w {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        "tenth" => 10,
        "eleventh" => 11,
        "twelfth" => 12,
        "thirteenth" => 13,
        "fourteenth" => 14,
        "fifteenth" => 15,
        "sixteenth" => 16,
        "seventeenth" => 17,
        "eighteenth" => 18,
        "nineteenth" => 19,
        "twentieth" => 20,
        "thirtieth" => 30,
        _ => return None,
    })
}

fn strto_unit(w: &str) -> Option<Unit> {
    Some(match w {
        "sec" | "secs" | "second" | "seconds" => Unit::Sec,
        "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" | "usec" | "usecs"
        | "microsecond" | "microseconds" => Unit::Sub,
        "min" | "mins" | "minute" | "minutes" => Unit::Min,
        "hour" | "hours" => Unit::Hour,
        "day" | "days" => Unit::Day,
        "week" | "weeks" => Unit::Week,
        "fortnight" | "fortnights" => Unit::Fortnight,
        "weekday" | "weekdays" => Unit::Weekday,
        "month" | "months" => Unit::Month,
        "year" | "years" => Unit::Year,
        _ => return None,
    })
}

fn strto_tz_abbr(w: &str) -> Option<i64> {
    // seconds east of UTC
    Some(match w {
        "utc" | "ut" | "gmt" | "z" | "zulu" => 0,
        "est" => -5 * 3600,
        "edt" => -4 * 3600,
        "cst" => -6 * 3600,
        "cdt" => -5 * 3600,
        "mst" => -7 * 3600,
        "mdt" => -6 * 3600,
        "pst" => -8 * 3600,
        "pdt" => -7 * 3600,
        "cet" => 3600,
        "cest" | "bst" | "ist" => 7200, // ist ambiguous; BST/CEST family
        _ => return None,
    })
}

fn year_2dig(n: i64) -> i64 {
    if n < 70 {
        2000 + n
    } else {
        1900 + n
    }
}

/// Parse `s` as a relative/absolute date-time per Zend's strtotime.
/// Returns the resulting Unix timestamp, or None (PHP false).
pub(crate) fn strtotime_parse(s: &str, base: i64) -> Option<i64> {
    let toks = strto_toks(s.as_bytes());
    if toks.is_empty() {
        // "" → false, but whitespace/empty-token-only → base unchanged
        return if s.is_empty() { None } else { Some(base) };
    }
    let mut p = StrtoP {
        dt: Dt::from_ts(base),
        tz_off: None,
        slot: None,
        post: Vec::new(),
        bag: Bag::default(),
        i: 0,
    };
    if !p.run(&toks) {
        return None;
    }
    let dt = p.resolve();
    Some(dt.days * 86400 + dt.secs - p.tz_off.unwrap_or(0))
}

impl StrtoP {
    fn at_i(&self, t: &[STok], k: usize) -> Option<(i64, usize)> {
        match t.get(self.i + k) {
            Some(STok::I(n, w)) => Some((*n, *w)),
            _ => None,
        }
    }
    fn at_c(&self, t: &[STok], k: usize, c: u8) -> bool {
        matches!(t.get(self.i + k), Some(STok::C(x)) if *x == c)
    }
    fn at_w(&self, t: &[STok], k: usize) -> Option<String> {
        match t.get(self.i + k) {
            Some(STok::W(w)) => Some(w.clone()),
            _ => None,
        }
    }

    fn set_slot(&mut self, dow: i64, beh: SlotBeh) {
        // last-seen dow, first-seen behavior (Zend quirk)
        match &mut self.slot {
            Some(s) => s.0 = dow,
            None => self.slot = Some((dow, beh)),
        }
        self.dt.secs = 0; // weekday specs reset the clock
    }

    fn set_date(&mut self, y: i64, m: i64, d: i64) {
        self.dt.set_ymd(y, m, d);
        self.dt.secs = 0;
    }
    fn set_clock(&mut self, h: i64, m: i64, s: i64) {
        self.dt.secs = 0;
        self.dt.add_secs(h * 3600 + m * 60 + s);
    }

    fn run(&mut self, t: &[STok]) -> bool {
        while self.i < t.len() {
            if !self.step(t) {
                return false;
            }
        }
        true
    }

    fn step(&mut self, t: &[STok]) -> bool {
        match &t[self.i] {
            STok::C(b'@') => {
                // @epoch, optionally negative
                let (neg, k) = if self.at_c(t, 1, b'-') {
                    (-1, 2)
                } else {
                    (1, 1)
                };
                if let Some((n, _)) = self.at_i(t, k) {
                    self.dt = Dt::from_ts(neg * n);
                    self.tz_off = Some(0);
                    self.i += k + 1;
                    true
                } else {
                    false
                }
            }
            STok::C(b',') | STok::C(b'(') | STok::C(b')') => {
                self.i += 1;
                true
            }
            STok::C(b'+') => self.sign(t, 1),
            STok::C(b'-') => self.sign(t, -1),
            STok::C(_) => false,
            STok::I(..) => self.number(t),
            STok::W(w) => self.word(t, &w.clone()),
        }
    }

    /// `+N`/`-N` — timezone offset or signed relative delta.
    fn sign(&mut self, t: &[STok], sgn: i64) -> bool {
        let Some((n, w)) = self.at_i(t, 1) else {
            return false;
        };
        // +I + W(unit|weekday) → signed relative delta
        if let Some(word) = self.at_w(t, 2) {
            if let Some(u) = strto_unit(&word) {
                self.bag.add(sgn * n, u);
                self.i += 3;
                return true;
            }
            // +N <non-unit word>: numeric tz + swallow the word
            let off = if w >= 3 {
                (n / 100) * 3600 + (n % 100) * 60
            } else {
                n * 3600
            };
            if self.tz_off.is_none() {
                self.tz_off = Some(sgn * off);
            }
            self.i += 3;
            return true;
        }
        // +HH:MM
        if self.at_c(t, 2, b':') {
            if let Some((m, _)) = self.at_i(t, 3) {
                if m < 60 {
                    if self.tz_off.is_none() {
                        self.tz_off = Some(sgn * (n * 3600 + m * 60));
                    }
                    self.i += 4;
                    return true;
                }
            }
            return false;
        }
        // +HHMM / +HMM / +H
        let off = if w >= 3 {
            (n / 100) * 3600 + (n % 100) * 60
        } else {
            n * 3600
        };
        if self.tz_off.is_none() {
            self.tz_off = Some(sgn * off);
        }
        self.i += 2;
        true
    }

    /// A bare integer token — dates, times, years.
    fn number(&mut self, t: &[STok]) -> bool {
        let (n, w) = match t[self.i] {
            STok::I(n, w) => (n, w),
            _ => return false,
        };
        // --- dates containing separators ---
        // Y-m-d / Y/m/d
        if w == 4 && self.at_c(t, 1, b'-') {
            if let Some((a, aw)) = self.at_i(t, 2) {
                if self.at_c(t, 3, b'-') {
                    if let Some((b, _)) = self.at_i(t, 4) {
                        // ISO week: Y-Www-d
                        // (handled below via at_w == "w")
                        if !(1..=12).contains(&a) || !(1..=31).contains(&b) {
                            return false;
                        }
                        self.set_date(n, a, b);
                        self.i += 5;
                        return true;
                    }
                    return false;
                }
                // Y-DDD ordinal day
                if aw == 3 {
                    if !(1..=366).contains(&a) {
                        return false;
                    }
                    self.set_date(n, 1, 1);
                    self.dt.days += a - 1;
                    self.i += 3;
                    return true;
                }
                // Y-m
                if aw <= 2 {
                    if !(1..=12).contains(&a) {
                        return false;
                    }
                    self.set_date(n, a, 1);
                    self.i += 3;
                    return true;
                }
                return false;
            }
            if self.at_w(t, 2).as_deref() == Some("w") {
                // Y-Www-d
                if let Some((ww, _)) = self.at_i(t, 3) {
                    if self.at_c(t, 4, b'-') {
                        if let Some((d, _)) = self.at_i(t, 5) {
                            if (1..=53).contains(&ww) && (1..=7).contains(&d) {
                                let jan4 = days_from_civil(n, 1, 4);
                                let mon = jan4 - (jan4 + 3).rem_euclid(7);
                                self.dt.days = mon + (ww - 1) * 7 + (d - 1);
                                self.dt.secs = 0;
                                self.i += 6;
                                return true;
                            }
                            return false;
                        }
                        return false;
                    }
                }
                return false;
            }
            return false;
        }
        // d-M-Y: 15-Jun-2025 (day - monthname - year)
        if self.at_c(t, 1, b'-') {
            if let Some(mn) = self.at_w(t, 2).as_deref().and_then(strto_month) {
                if self.at_c(t, 3, b'-') {
                    if let Some((y, yw)) = self.at_i(t, 4) {
                        if (yw == 4 || yw <= 2) && (1..=31).contains(&n) {
                            let y = if yw <= 2 { year_2dig(y) } else { y };
                            self.set_date(y, mn, n);
                            self.i += 5;
                            return true;
                        }
                        return false;
                    }
                    return false;
                }
            }
            if let Some((m, _)) = self.at_i(t, 2) {
                if self.at_c(t, 3, b'-') {
                    if let Some((y, yw)) = self.at_i(t, 4) {
                        if yw == 4 || yw <= 2 {
                            if !(1..=12).contains(&m) || !(1..=31).contains(&n) {
                                return false;
                            }
                            let y = if yw <= 2 { year_2dig(y) } else { y };
                            self.set_date(y, m, n);
                            self.i += 5;
                            return true;
                        }
                        return false;
                    }
                    return false;
                }
            }
            return false;
        }
        // m/d/Y (US) or Y/m/d
        if self.at_c(t, 1, b'/') {
            if let Some((b, _)) = self.at_i(t, 2) {
                if self.at_c(t, 3, b'/') {
                    if let Some((c, cw)) = self.at_i(t, 4) {
                        let (y, m, d) = if w == 4 {
                            (n, b, c) // Y/m/d
                        } else {
                            // m/d/Y
                            let y = if cw <= 2 { year_2dig(c) } else { c };
                            (y, n, b)
                        };
                        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                            return false;
                        }
                        self.set_date(y, m, d);
                        self.i += 5;
                        return true;
                    }
                    return false;
                }
                return false;
            }
            return false;
        }
        // d.m.Y / d.m.y / H.i / H.i.s
        if self.at_c(t, 1, b'.') {
            let Some((m, _)) = self.at_i(t, 2) else {
                return false;
            };
            if self.at_c(t, 3, b'.') {
                let Some((y, yw)) = self.at_i(t, 4) else {
                    return false;
                };
                // d.m.Y / d.m.y when the middle reads as a month
                if (1..=12).contains(&m) && (1..=31).contains(&n) && (yw == 4 || yw <= 2) {
                    let y = if yw <= 2 { year_2dig(y) } else { y };
                    self.set_date(y, m, n);
                    self.i += 5;
                    return true;
                }
                // H.i.s time
                if n <= 24 && m <= 59 && y <= 59 {
                    self.set_clock(n, m, y);
                    self.i += 5;
                    return true;
                }
                return false;
            }
            // H.i time (two parts only)
            if n <= 24 && m <= 59 {
                self.set_clock(n, m, 0);
                self.i += 3;
                return true;
            }
            return false;
        }
        // H:i[:s[.frac]] time
        if self.at_c(t, 1, b':') {
            let Some((m, _)) = self.at_i(t, 2) else {
                return false;
            };
            if m >= 60 || n > 24 {
                return false;
            }
            let mut h = n;
            let mut used = 3;
            let mut sec = 0;
            if self.at_c(t, 3, b':') {
                let Some((s, _)) = self.at_i(t, 4) else {
                    return false;
                };
                if s >= 60 {
                    return false;
                }
                sec = s;
                used = 5;
                // optional .fraction — ignored
                if self.at_c(t, 5, b'.') && self.at_i(t, 6).is_some() {
                    used = 7;
                }
            }
            if h == 24 && sec > 0 {
                return false;
            }
            // trailing am/pm: applies only to a 12h clock
            if let Some(ap) = self.at_w(t, used) {
                if ap == "am" || ap == "pm" {
                    if h > 12 {
                        return false;
                    }
                    if ap == "pm" && h != 12 {
                        h += 12;
                    }
                    if ap == "am" && h == 12 {
                        h = 0;
                    }
                    used += 1;
                }
            }
            self.set_clock(h, m, sec);
            self.i += used;
            return true;
        }
        // I W(am|pm) — meridiem hour, e.g. "5pm"
        if let Some(ap) = self.at_w(t, 1) {
            if ap == "am" || ap == "pm" {
                if n > 12 {
                    return false;
                }
                let h = if ap == "am" {
                    if n == 12 {
                        0
                    } else {
                        n
                    }
                } else if n == 12 {
                    12
                } else {
                    n + 12
                };
                self.set_clock(h, 0, 0);
                self.i += 2;
                return true;
            }
            // bare `N <unit>` → unsigned relative delta ("5 days")
            if let Some(u) = strto_unit(&ap) {
                self.bag.add(n, u);
                self.i += 2;
                return true;
            }
        }
        // I <ordinal-suffix> <month>: "15th june"
        if let Some(sfx) = self.at_w(t, 1) {
            if matches!(sfx.as_str(), "st" | "nd" | "rd" | "th") {
                if let Some(mw) = self.at_w(t, 2) {
                    if let Some(m) = strto_month(&mw) {
                        if !(1..=31).contains(&n) {
                            return false;
                        }
                        let (cy, _, _) = self.dt.ymd();
                        let mut y = cy;
                        let mut used = 3;
                        if let Some((yy, yw)) = self.at_i(t, 3) {
                            if yw == 4 || yw <= 2 {
                                y = if yw <= 2 { year_2dig(yy) } else { yy };
                                used = 4;
                            }
                        }
                        self.set_date(y, m, n);
                        self.i += used;
                        return true;
                    }
                    return false;
                }
                return false;
            }
        }
        // I <month>: "15 june"
        if let Some(mw) = self.at_w(t, 1) {
            if let Some(m) = strto_month(&mw) {
                // day may be 0 (overflows to previous month's last day)
                if !(0..=31).contains(&n) {
                    return false;
                }
                let (cy, _, _) = self.dt.ymd();
                let mut y = cy;
                let mut used = 2;
                if self.at_c(t, 2, b',') {
                    used = 3;
                }
                if let Some((yy, yw)) = self.at_i(t, used) {
                    if yw == 4 || yw <= 2 {
                        y = if yw <= 2 { year_2dig(yy) } else { yy };
                        used += 1;
                    }
                }
                self.set_date(y, m, n);
                self.i += used;
                return true;
            }
            return false;
        }
        // YYYYMMDD
        if w == 8 {
            let (y, m, d) = (n / 10000, n / 100 % 100, n % 100);
            if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                return false;
            }
            self.set_date(y, m, d);
            self.i += 1;
            return true;
        }
        // HHMMSS
        if w == 6 {
            let (h, m, s) = (n / 10000, n / 100 % 100, n % 100);
            if h > 24 || m >= 60 || s >= 60 || (h == 24 && s > 0) {
                return false;
            }
            self.set_clock(h, m, s);
            self.i += 1;
            return true;
        }
        // HHMM or 4-digit year
        if w == 4 {
            let (h, m) = (n / 100, n % 100);
            if h <= 24 && m <= 59 {
                // "2400"/"2413" → rolls into the next day
                self.set_clock(h, m, 0);
            } else {
                // bare year: set year, keep month/day/time
                let (_, cm, cd) = self.dt.ymd();
                self.dt.set_ymd(n, cm, cd);
            }
            self.i += 1;
            return true;
        }
        false
    }

    /// A word token.
    fn word(&mut self, t: &[STok], w: &str) -> bool {
        // months
        if let Some(m) = strto_month(w) {
            return self.month_spec(t, m);
        }
        // weekday names → dow slot
        if let Some(dow) = strto_dow(w) {
            self.set_slot(dow, SlotBeh::Bare);
            self.i += 1;
            return true;
        }
        if let Some(n) = strto_ord(w) {
            return self.ordinal(t, n);
        }
        match w {
            "now" => {
                self.i += 1;
                true
            }
            "today" | "midnight" => {
                self.dt.secs = 0;
                self.i += 1;
                true
            }
            "noon" => {
                self.set_clock(12, 0, 0);
                self.i += 1;
                true
            }
            "tomorrow" => {
                self.bag.set_day(1);
                self.i += 1;
                true
            }
            "yesterday" => {
                self.bag.set_day(-1);
                self.i += 1;
                true
            }
            "ago" => {
                self.bag.negate();
                for op in &mut self.post {
                    match op {
                        PostOp::SeekDow(n, _) | PostOp::Biz(n) | PostOp::WeekBound(n) => *n = -*n,
                    }
                }
                if let Some(s) = &mut self.slot {
                    s.1 = SlotBeh::Ago;
                }
                self.i += 1;
                true
            }
            "next" => self.nextish(t, 1),
            "this" => self.nextish(t, 0),
            "last" | "previous" => self.nextish(t, -1),
            "weekday" | "weekdays" => {
                // Zend quirk: bare "weekday" acts like next-Monday.
                self.set_slot(1, SlotBeh::Next);
                self.i += 1;
                true
            }
            "t" => {
                // ISO-8601 date/time separator — no-op
                self.i += 1;
                true
            }
            "back" | "front" => {
                // "back of 5pm" → 17:15; "front of 5pm" → 16:45
                if self.at_w(t, 1).as_deref() != Some("of") {
                    return false;
                }
                let dir = if w == "back" { 900 } else { -900 };
                match self.at_i(t, 2) {
                    Some((h, _)) => {
                        let mut hour = h;
                        let mut used = 3;
                        if let Some(ap) = self.at_w(t, 3) {
                            if ap == "am" || ap == "pm" {
                                if hour > 12 {
                                    return false;
                                }
                                if ap == "pm" && hour != 12 {
                                    hour += 12;
                                }
                                if ap == "am" && hour == 12 {
                                    hour = 0;
                                }
                                used = 4;
                            }
                        }
                        if hour > 24 {
                            return false;
                        }
                        self.set_clock(hour, 0, dir);
                        self.i += used;
                        true
                    }
                    _ => false,
                }
            }
            "a" => {
                // Zend quirk: "a" ≈ tz +01:00 and swallows one word.
                if self.tz_off.is_none() {
                    self.tz_off = Some(3600);
                }
                self.i += 1;
                if matches!(t.get(self.i), Some(STok::W(_))) {
                    self.i += 1;
                }
                true
            }
            _ => {
                // timezone abbreviations (+ gmt±N)
                if let Some(off) = strto_tz_abbr(w) {
                    if self.tz_off.is_none() {
                        self.tz_off = Some(off);
                    }
                    self.i += 1;
                    // GMT+5 / UTC-3
                    if w == "gmt" || w == "utc" {
                        if let Some(STok::C(sign)) = t.get(self.i) {
                            let sgn = if *sign == b'-' { -1 } else { 1 };
                            if matches!(*sign, b'+' | b'-') {
                                if let Some((n, w2)) = self.at_i(t, 1) {
                                    let off = if w2 >= 3 {
                                        (n / 100) * 3600 + (n % 100) * 60
                                    } else {
                                        n * 3600
                                    };
                                    self.tz_off = Some(sgn * off);
                                    self.i += 2;
                                }
                            }
                        }
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    /// `<month>` followed by optional day / year.
    fn month_spec(&mut self, t: &[STok], m: i64) -> bool {
        let (cy, _cm, cd) = self.dt.ymd();
        let mut y = cy;
        let mut d = cd; // bare "june" keeps the day-of-month
        let mut used = 1;
        if let Some((v, vw)) = self.at_i(t, used) {
            if vw == 4 {
                // year first: "june 2025" → day 1
                y = v;
                d = 1;
                used += 1;
            } else if v <= 31 {
                d = v;
                used += 1;
                // optional year after the day (possibly comma-separated)
                let mut k = used;
                if self.at_c(t, k, b',') {
                    k += 1;
                }
                if let Some((yy, yw)) = self.at_i(t, k) {
                    if yw == 4 || yw <= 2 {
                        y = if yw == 4 { yy } else { year_2dig(yy) };
                        used = k + 1;
                    }
                }
            }
        }
        self.set_date(y, m, d);
        self.i += used;
        true
    }

    /// `next|this|last|previous` + unit/weekday/dow.
    fn nextish(&mut self, t: &[STok], dir: i64) -> bool {
        match self.at_w(t, 1).as_deref() {
            Some("weekday") | Some("weekdays") => {
                let n = if dir == 0 { 0 } else { dir };
                self.dt.secs = 0;
                self.post.push(PostOp::Biz(n));
                self.i += 2;
                true
            }
            Some("week") => {
                self.post.push(PostOp::WeekBound(dir));
                self.i += 2;
                true
            }
            Some("day") => {
                if self.at_w(t, 2).as_deref() == Some("of") {
                    // only `last|previous day of <month>` — `next day of` fails
                    if dir >= 0 {
                        return false;
                    }
                    self.i += 3;
                    return self.of_clause(t, Some((-1, None)));
                }
                self.bag.add(dir, Unit::Day);
                self.i += 2;
                true
            }
            Some(word) => {
                if let Some(dow) = strto_dow(word) {
                    // `next friday of ...` → of-clause
                    if self.at_w(t, 2).as_deref() == Some("of") {
                        let n = if dir <= 0 { -1 } else { dir }; // next=1st, last/prev=last
                        self.i += 3;
                        return self.of_clause(t, Some((n, Some(dow))));
                    }
                    let beh = match dir {
                        1 => SlotBeh::Next,
                        0 => SlotBeh::Bare,
                        _ => SlotBeh::Last,
                    };
                    self.set_slot(dow, beh);
                    self.i += 2;
                    return true;
                }
                if let Some(u) = strto_unit(word) {
                    self.bag.add(dir, u);
                    self.i += 2;
                    return true;
                }
                false
            }
            None => false,
        }
    }

    /// `Nth` + day/weekday/dow/unit [+ "of" ...].
    fn ordinal(&mut self, t: &[STok], n: i64) -> bool {
        match self.at_w(t, 1).as_deref() {
            Some("day") => {
                if self.at_w(t, 2).as_deref() == Some("of") {
                    // only `first day of` — `second day of` & friends fail
                    if n != 1 {
                        return false;
                    }
                    self.i += 3;
                    return self.of_clause(t, Some((1, None)));
                }
                self.bag.add(n, Unit::Day);
                self.i += 2;
                true
            }
            Some("weekday") | Some("weekdays") => {
                // `Nth weekday of` is invalid in Zend — leave to fail via "of"
                self.dt.secs = 0;
                self.post.push(PostOp::Biz(n));
                self.i += 2;
                true
            }
            Some("week") => false, // "first week" → false
            Some(word) => {
                if let Some(dow) = strto_dow(word) {
                    if self.at_w(t, 2).as_deref() == Some("of") {
                        self.i += 3;
                        return self.of_clause(t, Some((n, Some(dow))));
                    }
                    self.dt.secs = 0;
                    self.post.push(PostOp::SeekDow(n, dow));
                    self.i += 2;
                    return true;
                }
                if let Some(u) = strto_unit(word) {
                    self.bag.add(n, u);
                    self.i += 2;
                    return true;
                }
                false
            }
            None => false,
        }
    }

    /// `... of <spec>`: parse the spec recursively and apply the
    /// day-of / nth-dow-of adjustment. Spec = the rest of the string.
    /// arg = (n, dow) — dow None → "Nth day of"; Some(dow) → "Nth dow of".
    fn of_clause(&mut self, t: &[STok], arg: Option<(i64, Option<i64>)>) -> bool {
        let Some((n, dow)) = arg else {
            return false;
        };
        let mut sub = StrtoP {
            dt: self.dt,
            tz_off: None,
            slot: None,
            post: Vec::new(),
            bag: Bag::default(),
            i: 0,
        };
        if !sub.run(&t[self.i..]) {
            return false;
        }
        let spec = sub.resolve();
        self.i = t.len();
        match dow {
            None => {
                // Nth day of <month>: day-of-month seek, spec's time kept.
                let (y, m, _) = spec.ymd();
                let day = if n <= 0 {
                    days_from_civil(
                        if m == 12 { y + 1 } else { y },
                        if m == 12 { 1 } else { m + 1 },
                        1,
                    ) - days_from_civil(y, m, 1) // last day of month
                } else {
                    n
                };
                self.dt = Dt {
                    days: days_from_civil(y, m, day),
                    secs: spec.secs,
                };
                true
            }
            Some(dw) => {
                // Nth <dow> of <month>: time reset.
                let (y, m, _) = spec.ymd();
                let first = days_from_civil(y, m, 1);
                let last_day = days_from_civil(
                    if m == 12 { y + 1 } else { y },
                    if m == 12 { 1 } else { m + 1 },
                    1,
                ) - 1;
                let day = if n > 0 {
                    let cur = (first + 4).rem_euclid(7); // dow of the 1st
                    first + (dw - cur).rem_euclid(7) + 7 * (n - 1)
                } else {
                    let cur = (last_day + 4).rem_euclid(7);
                    last_day - (cur - dw).rem_euclid(7)
                };
                if day < first || day > last_day {
                    return false;
                }
                self.dt = Dt { days: day, secs: 0 };
                true
            }
        }
    }

    /// Apply the collected slot, post-ops, then the bag.
    fn resolve(&self) -> Dt {
        let mut dt = self.dt;
        if let Some((dow, beh)) = self.slot {
            let cur = dt.dow();
            let delta = match beh {
                SlotBeh::Bare => (dow - cur).rem_euclid(7),
                SlotBeh::Next => (dow - cur - 1).rem_euclid(7) + 1,
                SlotBeh::Last => -((cur - dow - 1).rem_euclid(7) + 1),
                SlotBeh::Ago => -((cur - dow - 1).rem_euclid(7) + 1) - 7,
            };
            dt.days += delta;
        }
        for op in &self.post {
            match *op {
                PostOp::SeekDow(n, dow) => {
                    let fwd = n > 0;
                    for _ in 0..n.abs() {
                        let cur = dt.dow();
                        dt.days += if fwd {
                            (dow - cur - 1).rem_euclid(7) + 1
                        } else {
                            -((cur - dow - 1).rem_euclid(7) + 1)
                        };
                    }
                }
                PostOp::Biz(n) => {
                    if n == 0 {
                        // "this weekday": today if a weekday, else next one
                        if dt.dow() == 0 || dt.dow() == 6 {
                            dt.add_weekdays(1);
                        }
                    } else {
                        dt.add_weekdays(n);
                    }
                }
                PostOp::WeekBound(d) => {
                    let iso = dt.iso_dow();
                    match d {
                        1 => dt.days += 7 - iso, // next Monday
                        0 => dt.days -= iso,     // this week's Monday
                        _ => dt.days -= iso + 7, // previous Monday
                    }
                }
            }
        }
        // Bag: tomorrow/yesterday's reset applies before the deltas;
        // deltas apply in Zend's fixed y→m→d→weekday→time order.
        if self.bag.reset {
            dt.secs = 0;
        }
        dt.add_months(self.bag.months);
        dt.days += self.bag.days;
        if self.bag.biz != 0 {
            dt.add_weekdays(self.bag.biz);
        }
        dt.add_secs(self.bag.secs);
        dt
    }
}

/// `DateTime::createFromFormat` subset parser. Fields the format
/// doesn't set come from `now` (zend default); `!` resets them to
/// epoch inline, `|` resets whatever's still unset at the end, `+`
/// permits trailing data. `c`/`r` expand to their composite formats.
/// ponytail: `e`/`T`/`D`/`l`/`N`/`w`/`W`/`t` consume their input but
/// set nothing (UTC-only store — no tz names, weekday letters are
/// decorative); unknown directive letters act as literals, which is
/// zend's fallback for non-directive chars too.
/// Parsed field+display bundle: `ts` is the instant (wall-minus-
/// offset), `off` the seconds format() shifts by, `tzty`/`tz` zend's
/// var_dump timezone_type (1 = `±HH:MM`, 3 = named) + name.
pub(crate) struct DtNew {
    pub ts: i64,
    pub off: i64,
    pub tzty: i64,
    pub tz: String,
}

pub(crate) fn date_create_from_format(fmt: &str, val: &str, now: i64) -> Option<DtNew> {
    let first = cff_inner(fmt, val, now)?;
    // zend fills unparsed fields from `now` in the TARGET zone, so
    // run once to learn the offset, then again with it applied —
    // a no-op second pass when the input carries its own zoneless
    // fields or the zone is UTC.
    if first.off == 0 && first.tz == "UTC" {
        return Some(first);
    }
    cff_inner(fmt, val, now + first.off)
}

fn cff_inner(fmt: &str, val: &str, now: i64) -> Option<DtNew> {
    let (ny, nm, nd) = civil_from_days(now.div_euclid(86400));
    let nsec = now.rem_euclid(86400);
    // (y, m, d, h, i, s, off, z) — None = take from `now` unless reset.
    let mut f: [Option<i64>; 8] = [None; 8];
    let mut reset_seen = false;
    let mut twelve: Option<i64> = None;
    let mut meridiem: Option<bool> = None; // true = pm
    let mut epoch: Option<i64> = None;
    let mut tz_name: Option<String> = None;
    let mut off_named = true;
    let mut trailing_ok = false;
    let fb = fmt.as_bytes();
    let vb = val.as_bytes();
    let mut fi = 0usize;
    let mut vi = 0usize;
    let months: &[&[u8]] = &[
        b"january",
        b"february",
        b"march",
        b"april",
        b"may",
        b"june",
        b"july",
        b"august",
        b"september",
        b"october",
        b"november",
        b"december",
    ];
    let num = |vb: &[u8], vi: &mut usize, max: usize| -> Option<i64> {
        let start = *vi;
        while *vi - start < max && *vi < vb.len() && vb[*vi].is_ascii_digit() {
            *vi += 1;
        }
        if *vi == start {
            None
        } else {
            vb[start..*vi]
                .iter()
                .fold(0i64, |a, b| a * 10 + (b - b'0') as i64)
                .into()
        }
    };
    let word = |vb: &[u8], vi: &mut usize| -> Vec<u8> {
        let start = *vi;
        while *vi < vb.len() && vb[*vi].is_ascii_alphabetic() {
            *vi += 1;
        }
        vb[start..*vi].to_vec()
    };
    // one directive; returns false on hard mismatch
    macro_rules! need {
        ($e:expr) => {
            match $e {
                Some(x) => x,
                None => return None,
            }
        };
    }
    while fi < fb.len() {
        let c = fb[fi];
        fi += 1;
        match c {
            b'!' => {
                for x in f.iter_mut() {
                    *x = None;
                }
                twelve = None;
                meridiem = None;
                epoch = None;
                reset_seen = true;
            }
            b'|' => {
                // reset the UNPARSED fields to epoch — parsed stay
                reset_seen = true;
            }
            b'+' => trailing_ok = true,
            b'?' => {
                if vi >= vb.len() {
                    return None;
                }
                vi += 1;
            }
            b'#' => {
                if vi >= vb.len() || !b";:/.,-".contains(&vb[vi]) {
                    return None;
                }
                vi += 1;
            }
            b'*' => {
                // consume until the next fmt literal would match
                let next = fb.get(fi).copied();
                if let Some(nl) = next {
                    while vi < vb.len() && vb[vi] != nl {
                        vi += 1;
                    }
                } else {
                    vi = vb.len();
                }
            }
            b'\\' => {
                let l = *fb.get(fi)?;
                fi += 1;
                if vb.get(vi) != Some(&l) {
                    return None;
                }
                vi += 1;
            }
            b' ' => {
                while vi < vb.len() && vb[vi].is_ascii_whitespace() {
                    vi += 1;
                }
            }
            b'Y' | b'o' => f[0] = Some(need!(num(vb, &mut vi, 6))),
            b'y' => {
                let n = need!(num(vb, &mut vi, 2));
                f[0] = Some(if n < 70 { 2000 + n } else { 1900 + n });
            }
            b'm' | b'n' => {
                let n = need!(num(vb, &mut vi, 2));
                if !(1..=12).contains(&n) {
                    return None;
                }
                f[1] = Some(n);
            }
            b'M' | b'F' => {
                let w = word(vb, &mut vi);
                let lw: Vec<u8> = w.iter().map(|b| b.to_ascii_lowercase()).collect();
                let hit = months.iter().position(|m| {
                    let k = if c == b'M' { 3 } else { m.len() };
                    lw.len() >= 3
                        && lw[..k.min(lw.len())] == m[..k.min(lw.len())]
                        && if c == b'M' { lw.len() == 3 } else { lw == **m }
                });
                f[1] = Some(hit? as i64 + 1);
            }
            b'd' | b'j' => {
                let n = need!(num(vb, &mut vi, 2));
                if !(1..=31).contains(&n) {
                    return None;
                }
                f[2] = Some(n);
            }
            b'z' => f[7] = Some(need!(num(vb, &mut vi, 3))),
            b'D' | b'l' | b'N' | b'w' | b'W' | b't' | b'S' => {
                // weekday/month trivia — consume, set nothing
                let w = word(vb, &mut vi);
                if w.is_empty() && num(vb, &mut vi, 3).is_none() {
                    return None;
                }
            }
            b'H' | b'G' => {
                let n = need!(num(vb, &mut vi, 2));
                if n > 23 {
                    return None;
                }
                f[3] = Some(n);
            }
            b'h' | b'g' => {
                let n = need!(num(vb, &mut vi, 2));
                if !(1..=12).contains(&n) {
                    return None;
                }
                twelve = Some(n);
            }
            b'i' => {
                let n = need!(num(vb, &mut vi, 2));
                if n > 59 {
                    return None;
                }
                f[4] = Some(n);
            }
            b's' => {
                let n = need!(num(vb, &mut vi, 2));
                if n > 59 {
                    return None;
                }
                f[5] = Some(n);
            }
            b'u' => {
                let _ = num(vb, &mut vi, 6);
            }
            b'v' => {
                let _ = num(vb, &mut vi, 3);
            }
            b'a' | b'A' => {
                let w = word(vb, &mut vi);
                let lw: Vec<u8> = w.iter().map(|b| b.to_ascii_lowercase()).collect();
                match lw.as_slice() {
                    b"am" => meridiem = Some(false),
                    b"pm" => meridiem = Some(true),
                    _ => return None,
                }
            }
            b'U' => epoch = Some(need!(num(vb, &mut vi, 20))),
            b'e' | b'T' => {
                // timezone name — sets the render offset + display name
                let st = vi;
                while vi < vb.len()
                    && (vb[vi].is_ascii_alphanumeric()
                        || matches!(vb[vi], b'/' | b'_' | b'+' | b'-' | b':'))
                {
                    vi += 1;
                }
                if vi == st {
                    return None;
                }
                tz_name = Some(String::from_utf8_lossy(&vb[st..vi]).to_string());
            }
            b'O' | b'P' => {
                off_named = false;
                let sgn = match vb.get(vi) {
                    Some(b'+') => 1,
                    Some(b'-') => -1,
                    _ => return None,
                };
                vi += 1;
                let h = need!(num(vb, &mut vi, 2));
                if c == b'P' {
                    if vb.get(vi) != Some(&b':') {
                        return None;
                    }
                    vi += 1;
                }
                let m = need!(num(vb, &mut vi, 2));
                f[6] = Some(sgn * (h * 3600 + m * 60));
            }
            b'Z' => {
                // oracle rejects Z in createFromFormat outright
                return None;
            }
            b'c' => return date_create_from_format("Y-m-d\\TH:i:sP", val, now),
            b'r' => return date_create_from_format("D, d M Y H:i:s O", val, now),
            _ => {
                if vb.get(vi) != Some(&c) {
                    return None;
                }
                vi += 1;
            }
        }
    }
    // trailing input is an error unless `+` permitted it
    if vi < vb.len() && !trailing_ok {
        return None;
    }
    if let Some(u) = epoch {
        return Some(DtNew {
            ts: u,
            off: 0,
            tzty: 3,
            tz: "UTC".into(),
        });
    }
    // Field-wise defaults: unset date fields come from today, and
    // unset time fields come from now — but only when NO time field
    // was parsed at all; one time field present zeroes the rest.
    let saw_time = f[3].is_some() || f[4].is_some() || f[5].is_some() || twelve.is_some();
    let base: [i64; 8] = if reset_seen {
        [1970, 1, 1, 0, 0, 0, 0, 0]
    } else if saw_time {
        [ny, nm, nd, 0, 0, 0, 0, 0]
    } else {
        [ny, nm, nd, nsec / 3600, nsec % 3600 / 60, nsec % 60, 0, 0]
    };
    let get = |i: usize| f[i].unwrap_or(base[i]);
    let h = match (twelve, meridiem) {
        (Some(t), Some(true)) => t % 12 + 12,
        (Some(t), Some(false)) => t % 12,
        (Some(t), None) => t % 24,
        (None, _) => get(3),
    };
    let days = if f[7].is_some() {
        days_from_civil(get(0), 1, 1) + get(7)
    } else {
        days_from_civil(get(0), get(1), get(2))
    };
    let wall = days * 86400 + h * 3600 + get(4) * 60 + get(5);
    match (tz_name, off_named) {
        (Some(name), true) => {
            let off = tz_offset_at(&name, wall);
            Some(DtNew {
                ts: wall - off,
                off,
                tzty: 3,
                tz: name,
            })
        }
        _ => {
            let off = get(6);
            let (tzty, tz) = if f[6].is_some() {
                // canonical `±HH:MM`
                let sgn = if off < 0 { '-' } else { '+' };
                (
                    1,
                    format!(
                        "{}{:02}:{:02}",
                        sgn,
                        off.abs() / 3600,
                        off.abs() % 3600 / 60
                    ),
                )
            } else {
                (3, "UTC".to_string())
            };
            Some(DtNew {
                ts: wall - off,
                off,
                tzty,
                tz,
            })
        }
    }
}

/// tz abbreviation ("JST", "UTC") of a named zone at `ts` — same
/// libc/TZ swap as tz_offset_at.
pub(crate) fn tz_abbr_at(name: &str, ts: i64) -> String {
    let b = name.as_bytes();
    if b.len() == 6 && matches!(b[0], b'+' | b'-') && b[3] == b':' {
        return name.to_string();
    }
    extern "C" {
        fn tzset();
    }
    let old = std::env::var("TZ").ok();
    std::env::set_var("TZ", name);
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let out = unsafe {
        tzset();
        if libc::localtime_r(&t, &mut tm).is_null() || tm.tm_zone.is_null() {
            name.to_string()
        } else {
            std::ffi::CStr::from_ptr(tm.tm_zone)
                .to_string_lossy()
                .to_string()
        }
    };
    match old {
        Some(v) => std::env::set_var("TZ", v),
        None => std::env::remove_var("TZ"),
    }
    unsafe { tzset() };
    out
}

/// UTC offset (seconds) of a named zone at `ts`. `±HH:MM` is
/// arithmetic; `Area/City` and legacy names go through libc's
/// `localtime_r` under a temporary `TZ=` — the system zoneinfo db is
/// already on the box, no tz tables to ship.
/// ponytail: `TZ` is process-global — fine while the interp is
/// single-threaded; a threaded VM needs a lock around the swap.
pub(crate) fn tz_offset_at(name: &str, ts: i64) -> i64 {
    let b = name.as_bytes();
    if b.len() == 6
        && matches!(b[0], b'+' | b'-')
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3] == b':'
        && b[4].is_ascii_digit()
        && b[5].is_ascii_digit()
    {
        let off = (b[1] - b'0') as i64 * 36000
            + (b[2] - b'0') as i64 * 3600
            + (b[4] - b'0') as i64 * 600
            + (b[5] - b'0') as i64 * 60;
        return if b[0] == b'+' { off } else { -off };
    }
    extern "C" {
        // not in the libc crate; glibc needs it to notice TZ changes
        fn tzset();
    }
    let old = std::env::var("TZ").ok();
    std::env::set_var("TZ", name);
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let off = unsafe {
        tzset();
        if libc::localtime_r(&t, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff
        }
    };
    match old {
        Some(v) => std::env::set_var("TZ", v),
        None => std::env::remove_var("TZ"),
    }
    unsafe { tzset() };
    off
}
