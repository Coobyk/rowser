fn main() {
    let u = rowser_net::Url::parse("https://example.com/").unwrap();
    println!("host={:?} port={} scheme={:?} target={:?}", u.host, u.port, u.scheme, u.request_target());
}
