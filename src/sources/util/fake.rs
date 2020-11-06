use chrono::prelude::Local;
use fakedata_generator::{gen_http_method, gen_ipv4, gen_username};
use rand::{thread_rng, Rng};

pub fn apache_common_log_line() -> String {
    // Example log line:
    // 173.159.239.159 - schoen1464 [31/Oct/2020:19:06:10 -0700] "POST /wireless HTTP/2.0" 100 20815
    format!(
        "{} - {} [{}] \"{} {} {}\" {} {}",
        ipv4_address(),
        username(),
        timestamp_apache_common(),
        http_method(),
        http_endpoint(),
        http_version(),
        http_code(),
        byte_size(),
    )
}

pub fn apache_error_log_line() -> String {
    // Example log line:
    // [Sat Oct 31 19:27:55 2020] [deleniti:crit] [pid 879:tid 9607] [client 169.198.228.174:1364] Try to program the IB port, maybe it will hack the online transmitter!

    format!(
        "[{}] [{}:{}] [pid {}:tid] [client {}:{}] {}",
        timestamp_apache_error(),
        username(),
        error_level(),
        pid(),
        ipv4_address(),
        port(),
        message(),
    )
}

// Formatted timestamps
fn timestamp_apache_common() -> String {
    Local::now().format("%d/%b/%Y:%T %z").to_string()
}

fn timestamp_apache_error() -> String {
    Local::now().format("%a %b %d %T %T").to_string()
}

// Other random strings
fn error_level() -> String {
    let levels: Vec<&'static str> = vec![
        "alert", "crit", "debug", "emerg", "error", "info", "notice", "trace1-8", "warn",
    ];
    random_from_vec(levels).to_string()
}

fn error_message() -> String {
    let messages: Vec<&'static str> = vec!["Something bad happened"];
    random_from_vec(messages).to_string()
}

fn http_code() -> String {
    let codes: Vec<usize> = vec![
        200, 300, 301, 302, 304, 307, 400, 401, 403, 404, 410, 500, 501, 503, 550,
    ];

    random_from_vec(codes).to_string()
}

fn byte_size() -> String {
    random_in_range(50, 50000)
}

fn http_endpoint() -> String {
    let endpoints: Vec<&'static str> = vec!["/foo", "/bar"];
    random_from_vec(endpoints).into()
}

fn http_method() -> String {
    gen_http_method()
}

fn http_version() -> String {
    let versions: Vec<&'static str> = vec!["HTTP/1.0", "HTTP/1.1", "HTTP/2.0"];
    random_from_vec(versions).into()
}

fn ipv4_address() -> String {
    gen_ipv4()
}

fn message() -> String {
    let messages: Vec<&'static str> = vec!["something went wrong", "oops"];
    random_from_vec(messages).into()
}

fn pid() -> String {
    random_in_range(1, 9999)
}

fn port() -> String {
    random_in_range(1024, 65535)
}

fn username() -> String {
    gen_username()
}

// Helper functions
fn random_in_range(min: usize, max: usize) -> String {
    thread_rng().gen_range(min, max).to_string()
}

fn random_from_vec<T: Copy>(v: Vec<T>) -> T {
    v[thread_rng().gen_range(0, v.len())]
}
