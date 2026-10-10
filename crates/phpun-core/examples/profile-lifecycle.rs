//! Decomposes the explicit persistent-worker lifecycle; no default mode changes.
use phpun_core::{
    interp::CallArgs,
    parser,
    value::{ArrKey, Cell, PhpArray, TraceFrame, Value},
    Interp,
};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn main() {
    let reps: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "200".into())
        .parse()
        .unwrap();
    assert!(reps > 0);
    let source = std::fs::read_to_string("bench/http/app-worker.php").unwrap();
    let mut new = Duration::ZERO;
    let mut parse = Duration::ZERO;
    let mut bootstrap = Duration::ZERO;
    for _ in 0..reps {
        let start = Instant::now();
        let mut it = Interp::new("bench/http/app-worker.php");
        new += start.elapsed();
        let start = Instant::now();
        std::hint::black_box(parser::parse_source(&source, false).unwrap());
        parse += start.elapsed();
        let start = Instant::now();
        let (result, handler) = it.run_source_ret(&source);
        assert_eq!(result.exit_code, 0);
        assert!(handler.is_some());
        bootstrap += start.elapsed(); // Includes parsing; not additive with parse.
    }
    let mut it = Interp::new("bench/http/app-worker.php");
    let (result, handler) = it.run_source_ret(&source);
    assert_eq!(result.exit_code, 0);
    let handler = handler.unwrap();
    it.seal_boot_objects();
    let mut reset = Duration::ZERO;
    let mut request = Duration::ZERO;
    let mut end = Duration::ZERO;
    let mut oracle = None;
    for _ in 0..reps {
        let start = Instant::now();
        it.reset_request();
        reset += start.elapsed();
        let mut req = PhpArray::new();
        req.set(ArrKey::Str("query".into()), Value::str("name=alice"));
        let args = CallArgs::positional(vec![Rc::new(RefCell::new(Value::Array(Rc::new(
            RefCell::new(req),
        ))))]);
        let start = Instant::now();
        let response = it.call_value(&handler, args).unwrap();
        request += start.elapsed();
        let Value::Array(response) = response else {
            panic!("handler response");
        };
        let body = response.borrow().get(&ArrKey::Str("body".into()));
        let body = body.expect("response body").to_php_string();
        assert!(body.starts_with("bench-ok:"));
        assert_eq!(oracle.get_or_insert_with(|| body.clone()), &body);
        let start = Instant::now();
        it.end_request();
        end += start.elapsed();
        assert!(it.err_buf.is_empty());
    }
    for (label, elapsed) in [
        ("Interp::new", new),
        ("parse", parse),
        ("bootstrap_including_parse", bootstrap),
        ("reset_request", reset),
        ("handler", request),
        ("end_request", end),
    ] {
        println!(
            "{label}: {:.3} us/op ({reps} reps)",
            elapsed.as_secs_f64() * 1e6 / reps as f64
        );
    }
    println!(
        "layout bytes: Value={} Cell={} TraceFrame={}",
        std::mem::size_of::<Value>(),
        std::mem::size_of::<Cell>(),
        std::mem::size_of::<TraceFrame>()
    );
    println!("response: {}", oracle.unwrap());
}
