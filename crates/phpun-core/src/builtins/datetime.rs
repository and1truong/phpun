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
        "strtotime" => Value::Int(0), // TODO real parsing
        "date_parse" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "microtime_float" => Value::Float(0.0),
        "date_sunrise" | "date_sunset" | "date_sun_info" => Value::Bool(false),
        _ => return Ok(None),
    }))
}

// ----- helpers -----

pub(in crate::builtins) fn date_format(fmt: &str, ts: i64) -> String {
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
            'e' | 'T' => out.push_str("UTC"),
            'O' => out.push_str("+0000"),
            'P' => out.push_str("+00:00"),
            'c' => out.push_str(&format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
                y, m, d, h, mi, s
            )),
            'r' => out.push_str(&format!(
                "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
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
                s
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
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
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
