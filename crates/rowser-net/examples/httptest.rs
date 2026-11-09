fn main() {
    match rowser_net::fetch("http://example.com") {
        Ok(r) => println!("status={} bytes={} url={}", r.status, r.body.len(), r.url),
        Err(e) => println!("ERR {e}"),
    }
}
