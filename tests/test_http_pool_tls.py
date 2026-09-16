"""Real TLS checks for concurrent pooled connections and certificate rejection."""

import datetime
import ssl
import tempfile
import threading
import unittest
import warnings
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch
from urllib.request import Request
from urllib.error import URLError

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID
import truststore
import urllib3.connection

from easy_multi_provider import http_pool


class PoolTLSTests(unittest.TestCase):
    def test_concurrent_verified_connections_reject_untrusted_and_wrong_host(self):
        key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        ca_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
        ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "EMP test CA")])
        now = datetime.datetime.now(datetime.timezone.utc)
        ca = (x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name)
              .public_key(ca_key.public_key()).serial_number(x509.random_serial_number())
              .not_valid_before(now - datetime.timedelta(minutes=1))
              .not_valid_after(now + datetime.timedelta(days=1))
              .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
              .add_extension(x509.KeyUsage(digital_signature=True, content_commitment=False,
                  key_encipherment=False, data_encipherment=False, key_agreement=False,
                  key_cert_sign=True, crl_sign=True, encipher_only=None, decipher_only=None), critical=True)
              .add_extension(x509.SubjectKeyIdentifier.from_public_key(ca_key.public_key()), critical=False)
              .sign(ca_key, hashes.SHA256()))
        cert = (x509.CertificateBuilder().subject_name(name).issuer_name(ca_name)
                .public_key(key.public_key()).serial_number(x509.random_serial_number())
                .not_valid_before(now - datetime.timedelta(minutes=1))
                .not_valid_after(now + datetime.timedelta(days=1))
                .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
                .add_extension(x509.KeyUsage(digital_signature=True, content_commitment=False,
                    key_encipherment=True, data_encipherment=False, key_agreement=False,
                    key_cert_sign=False, crl_sign=False, encipher_only=None, decipher_only=None), critical=True)
                .add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
                .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
                .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False)
                .sign(ca_key, hashes.SHA256()))

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "ca.pem").write_bytes(ca.public_bytes(serialization.Encoding.PEM))
            (root / "cert.pem").write_bytes(cert.public_bytes(serialization.Encoding.PEM))
            (root / "key.pem").write_bytes(key.private_bytes(serialization.Encoding.PEM,
                serialization.PrivateFormat.TraditionalOpenSSL, serialization.NoEncryption()))
            server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(root / "cert.pem", root / "key.pem")
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            contexts = []
            factory = urllib3.connection.create_urllib3_context

            def create_context(*args, **kwargs):
                value = factory(*args, **kwargs)
                contexts.append(value)
                return value

            def fetch(host):
                with http_pool.open_request(Request(f"https://{host}:{server.server_port}/"), 5) as response:
                    return response.read()

            try:
                http_pool.close_pools()
                with patch.object(http_pool, "proxy_for_url", return_value=None), \
                     patch("urllib3.util.ssl_.SSLContext", truststore.SSLContext), \
                     patch("urllib3.connection.create_urllib3_context", side_effect=create_context), \
                     warnings.catch_warnings():
                    warnings.simplefilter("error", urllib3.exceptions.InsecureRequestWarning)
                    with self.assertRaises(URLError):
                        fetch("localhost")
                    http_pool.close_pools()
                    http_pool._manager(None).connection_pool_kw["ca_certs"] = str(root / "ca.pem")
                    with ThreadPoolExecutor(max_workers=4) as pool:
                        self.assertEqual(list(pool.map(fetch, ["localhost"] * 8)), [b"ok"] * 8)
                    with self.assertRaises(URLError):
                        fetch("127.0.0.1")
                    self.assertGreaterEqual(len(contexts), 10)
                    self.assertEqual(len(contexts), len({id(c) for c in contexts}))
                    self.assertTrue(all(c.verify_mode == ssl.CERT_REQUIRED for c in contexts))
                    self.assertTrue(all(c.check_hostname for c in contexts))
            finally:
                http_pool.close_pools()
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)
