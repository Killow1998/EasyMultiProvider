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

#[test]
fn authenticated_receiver_requires_consent_and_consumes_code_once() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let code = Code::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()).unwrap();
    let delivery = Arc::new(Mutex::new(Delivery {
        status: "waiting",
        receiver: None,
        approved: false,
        bundle: Some(b"encrypted fixture".to_vec()),
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let worker = serve(
        listener,
        code.clone(),
        Instant::now() + Duration::from_secs(10),
        delivery.clone(),
        cancel,
    );
    let mut wrong = code.clone();
    wrong.0[7] ^= 1;
    assert!(receive(&wrong, &[1; 32]).is_err());
    assert!(delivery.lock().unwrap().receiver.is_none());
    assert!(receive(&code, &[1; 32]).unwrap().is_none());
    assert!(receive(&code, &[2; 32]).is_err());
    delivery.lock().unwrap().approved = true;
    assert_eq!(
        receive(&code, &[1; 32]).unwrap(),
        Some(b"encrypted fixture".to_vec())
    );
    worker.join().unwrap();
    assert!(receive(&code, &[1; 32]).is_err());
    assert!(delivery.lock().unwrap().bundle.is_none());
}

#[test]
fn expiry_closes_listener_and_drops_encrypted_payload() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let code = Code::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()).unwrap();
    let delivery = Arc::new(Mutex::new(Delivery {
        status: "waiting",
        receiver: None,
        approved: false,
        bundle: Some(vec![1; 32]),
    }));
    let worker = serve(
        listener,
        code.clone(),
        Instant::now() + Duration::from_millis(50),
        delivery.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    worker.join().unwrap();
    assert_eq!(delivery.lock().unwrap().status, "expired");
    assert!(delivery.lock().unwrap().bundle.is_none());
    assert!(receive(&code, &[1; 32]).is_err());
}
