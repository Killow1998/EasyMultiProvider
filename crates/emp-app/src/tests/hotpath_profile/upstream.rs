use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub(super) struct Upstream {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    count: Arc<AtomicUsize>,
    worker: Option<JoinHandle<()>>,
}
impl Upstream {
    pub(super) fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::clone(&stop);
        let counted = Arc::clone(&count);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let (mut stream, _) = listener.accept().unwrap();
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let (path, _, body) = receive_upstream_request(&mut stream);
                assert_eq!(body["model"], "upstream");
                let (content_type, response) = if path.ends_with("/chat/completions") {
                    let mut events = String::new();
                    for _ in 0..64 {
                        events.push_str(&format!("data: {}\n\n", json!({"id":"fixture",
                            "choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]})));
                    }
                    events.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
                    ("text/event-stream", events)
                } else {
                    (
                        "application/json",
                        json!({"id":"fixture","object":"response",
                        "status":"completed","model":"upstream","output":[],"future":{"keep":true}})
                        .to_string(),
                    )
                };
                counted.fetch_add(1, Ordering::AcqRel);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
            }
        });
        Self {
            address,
            stop,
            count,
            worker: Some(worker),
        }
    }
    pub(super) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    pub(super) fn requests(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        self.worker.take().unwrap().join().unwrap();
    }
}
