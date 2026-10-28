use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

pub(super) const MAX_BUNDLE: usize = 32 * 1024 * 1024;
const MAGIC: &[u8; 10] = b"EMP-LAN-1\n";
#[derive(Clone)]
pub(super) struct Code([u8; 36]);
pub(super) fn local_address(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback()
}
impl Code {
    pub(super) fn new(ip: Ipv4Addr, port: u16) -> Result<Self, &'static str> {
        let mut bytes = [0; 36];
        getrandom::getrandom(&mut bytes).map_err(|_| "lan_random_failed")?;
        bytes[0] = 1;
        bytes[1..5].copy_from_slice(&ip.octets());
        bytes[5..7].copy_from_slice(&port.to_be_bytes());
        Ok(Self(bytes))
    }
    pub(super) fn parse(text: &str) -> Result<Self, &'static str> {
        if text.len() != 49 || !text.starts_with('E') {
            return Err("lan_invalid_code");
        }
        let bytes: [u8; 36] = URL_SAFE_NO_PAD
            .decode(&text[1..])
            .map_err(|_| "lan_invalid_code")?
            .try_into()
            .map_err(|_| "lan_invalid_code")?;
        let code = Self(bytes);
        if bytes[0] != 1 || !local_address(code.ip()) || code.port() == 0 {
            return Err("lan_invalid_code");
        }
        Ok(code)
    }
    fn ip(&self) -> Ipv4Addr {
        Ipv4Addr::new(self.0[1], self.0[2], self.0[3], self.0[4])
    }
    fn port(&self) -> u16 {
        u16::from_be_bytes([self.0[5], self.0[6]])
    }
    pub(super) fn encode(&self) -> String {
        format!("E{}", URL_SAFE_NO_PAD.encode(self.0))
    }
    pub(super) fn password(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0[7..])
    }
    fn proof(&self, nonce: &[u8; 32]) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0[7..]).expect("HMAC key");
        mac.update(MAGIC);
        mac.update(&self.0[..7]);
        mac.update(nonce);
        mac
    }
}
pub(super) fn nonce(value: &str) -> Result<[u8; 32], &'static str> {
    if value.len() != 64 || !value.is_ascii() {
        return Err("lan_invalid_receiver");
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "lan_invalid_receiver")?;
    }
    Ok(bytes)
}

pub(super) fn receive(code: &Code, nonce: &[u8; 32]) -> Result<Option<Vec<u8>>, &'static str> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((code.ip(), code.port())),
        Duration::from_secs(3),
    )
    .map_err(|_| "lan_unreachable")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| "lan_unreachable")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| "lan_unreachable")?;
    let mut proof = MAGIC.to_vec();
    proof.extend(nonce);
    proof.extend(code.proof(nonce).finalize().into_bytes());
    stream.write_all(&proof).map_err(|_| "lan_unreachable")?;
    let mut status = [0];
    stream.read_exact(&mut status).map_err(|_| "lan_expired")?;
    if status == *b"P" {
        return Ok(None);
    }
    if status != *b"D" {
        return Err("lan_expired");
    }
    let mut length = [0; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| "lan_transfer_failed")?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_BUNDLE {
        return Err("lan_package_too_large");
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut data = vec![0; length];
    let mut count = 0;
    while count < length {
        if Instant::now() >= deadline {
            return Err("lan_transfer_failed");
        }
        let n = stream
            .read(&mut data[count..])
            .map_err(|_| "lan_transfer_failed")?;
        if n == 0 {
            return Err("lan_transfer_failed");
        }
        count += n;
    }
    Ok(Some(data))
}

pub(super) fn serve(
    listener: TcpListener,
    code: Code,
    expires: Instant,
    shared: Arc<Mutex<Delivery>>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while !cancel.load(Ordering::Acquire) && Instant::now() < expires {
            match listener.accept() {
                Ok((mut stream, peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    let mut request = [0; 74];
                    if read_request(&mut stream, &mut request, &cancel).is_err()
                        || &request[..10] != MAGIC
                    {
                        continue;
                    }
                    let nonce: [u8; 32] = request[10..42].try_into().unwrap();
                    if code.proof(&nonce).verify_slice(&request[42..]).is_err() {
                        continue;
                    }
                    let mut delivery = shared.lock().unwrap();
                    if cancel.load(Ordering::Acquire) || Instant::now() >= expires {
                        break;
                    }
                    if delivery.receiver.is_none() {
                        delivery.receiver = Some((peer.ip(), nonce));
                        delivery.status = "confirmation";
                    }
                    if delivery.receiver != Some((peer.ip(), nonce)) {
                        let _ = stream.write_all(b"X");
                        continue;
                    }
                    if !delivery.approved {
                        let _ = stream.write_all(b"P");
                        continue;
                    }
                    let Some(bundle) = delivery.bundle.take() else {
                        break;
                    };
                    delivery.status = "sending";
                    drop(delivery);
                    let result = send_bundle(&mut stream, &bundle, &cancel, expires);
                    shared.lock().unwrap().status = if result.is_ok() { "sent" } else { "failed" };
                    cancel.store(true, Ordering::Release);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(_) => break,
            }
        }
        let mut delivery = shared.lock().unwrap();
        delivery.bundle = None;
        delivery.status = "expired";
    })
}

fn read_request(
    stream: &mut TcpStream,
    request: &mut [u8],
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut received = 0;
    while received < request.len() {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let count = stream.read(&mut request[received..])?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        received += count;
    }
    Ok(())
}

fn send_bundle(
    stream: &mut TcpStream,
    bundle: &[u8],
    cancel: &AtomicBool,
    expires: Instant,
) -> std::io::Result<()> {
    stream.write_all(b"D")?;
    stream.write_all(&(bundle.len() as u32).to_be_bytes())?;
    let deadline = expires.min(Instant::now() + Duration::from_secs(30));
    let mut sent = 0;
    while sent < bundle.len() {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let count = stream.write(&bundle[sent..])?;
        if count == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        sent += count;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
