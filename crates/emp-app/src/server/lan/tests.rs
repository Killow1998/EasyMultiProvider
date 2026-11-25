use super::*;

#[test]
fn codes_require_a_private_endpoint_and_exact_ascii_nonce() {
    let code = Code::new(Ipv4Addr::LOCALHOST, 10000).unwrap();
    assert_eq!(code.encode().len(), 49);
    assert!(Code::parse(&code.encode()).is_ok());
    assert!(Code::parse(&Code::new(Ipv4Addr::new(8, 8, 8, 8), 443).unwrap().encode()).is_err());
    assert!(Code::parse(&Code::new(Ipv4Addr::LOCALHOST, 0).unwrap().encode()).is_err());
    assert!(nonce(&"é".repeat(32)).is_err());
}
