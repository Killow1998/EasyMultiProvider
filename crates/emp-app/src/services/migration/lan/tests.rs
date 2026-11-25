use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

#[test]
fn authenticated_receiver_requires_consent_and_consumes_code_once() {
    let (listener, port) = wire::Listener::bind(Ipv4Addr::LOCALHOST).ok().unwrap();
    let code = wire::Code::new(Ipv4Addr::LOCALHOST, port).unwrap();
    let delivery = Arc::new(Mutex::new(Delivery {
        status: "waiting",
        receiver: None,
        approved: false,
        bundle: Some(b"encrypted fixture".to_vec()),
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let worker = wire::serve(
        listener,
        code.clone(),
        Instant::now() + Duration::from_secs(10),
        delivery.clone(),
        cancel,
    );
    let mut forged = URL_SAFE_NO_PAD.decode(&code.encode()[1..]).unwrap();
    forged[7] ^= 1;
    let wrong = wire::Code::parse(&format!("E{}", URL_SAFE_NO_PAD.encode(forged))).unwrap();
    assert!(wire::receive(&wrong, &[1; 32]).is_err());
    assert!(delivery.lock().unwrap().receiver.is_none());
    assert!(wire::receive(&code, &[1; 32]).unwrap().is_none());
    assert!(wire::receive(&code, &[2; 32]).is_err());
    delivery.lock().unwrap().approved = true;
    assert_eq!(
        wire::receive(&code, &[1; 32]).unwrap(),
        Some(b"encrypted fixture".to_vec())
    );
    worker.join().unwrap();
    assert!(wire::receive(&code, &[1; 32]).is_err());
    assert!(delivery.lock().unwrap().bundle.is_none());
}

#[test]
fn expiry_closes_listener_and_drops_encrypted_payload() {
    let (listener, port) = wire::Listener::bind(Ipv4Addr::LOCALHOST).ok().unwrap();
    let code = wire::Code::new(Ipv4Addr::LOCALHOST, port).unwrap();
    let delivery = Arc::new(Mutex::new(Delivery {
        status: "waiting",
        receiver: None,
        approved: false,
        bundle: Some(vec![1; 32]),
    }));
    let worker = wire::serve(
        listener,
        code.clone(),
        Instant::now() + Duration::from_millis(50),
        delivery.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    worker.join().unwrap();
    assert_eq!(delivery.lock().unwrap().status, "expired");
    assert!(delivery.lock().unwrap().bundle.is_none());
    assert!(wire::receive(&code, &[1; 32]).is_err());
}
