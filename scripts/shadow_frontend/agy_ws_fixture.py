"""Bounded loopback WebSocket client for isolated agy acceptance."""
import base64, hashlib, json, os, re, socket, struct, time, uuid
from urllib.parse import urlsplit

def safe_code(value):
    return value if isinstance(value,str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,80}",value) else None

def recv_exact(sock,n):
    out=bytearray()
    while len(out)<n:
        data=sock.recv(n-len(out))
        if not data: raise EOFError('closed')
        out.extend(data)
    return bytes(out)
class WS:
    def __init__(self,base,token):
        u=urlsplit(base);assert u.hostname=='127.0.0.1'
        self.sock=socket.create_connection((u.hostname,u.port),timeout=90)
        key=base64.b64encode(os.urandom(16)).decode();scope=str(uuid.uuid4())
        headers={'Host':u.netloc,'Upgrade':'websocket','Connection':'Upgrade','Sec-WebSocket-Version':'13','Sec-WebSocket-Key':key,'X-EMP-Session':token,'Origin':base,'thread-id':scope,'session-id':scope,'OpenAI-Beta':'responses_websockets=2026-02-06','originator':'codex_cli_rs'}
        self.sock.sendall(('GET /v1/responses HTTP/1.1\r\n'+''.join(k+': '+v+'\r\n' for k,v in headers.items())+'\r\n').encode())
        head=bytearray()
        while not head.endswith(b'\r\n\r\n'):
            assert len(head)<65536;head.extend(recv_exact(self.sock,1))
        assert int(head.split(b' ')[1])==101,'local_ws_upgrade'
        accept=base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        assert accept in head
    def frame(self,opcode,data):
        mask=os.urandom(4);n=len(data);head=bytes([0x80|opcode])
        head+=bytes([0x80|n]) if n<126 else b'\xfe'+struct.pack('!H',n) if n<65536 else b'\xff'+struct.pack('!Q',n)
        self.sock.sendall(head+mask+bytes(b^mask[i%4] for i,b in enumerate(data)))
    def turn(self,body,marker):
        raw=json.dumps(body,separators=(',',':')).encode();self.frame(1,raw)
        start=time.monotonic();text='';count=0;content=bytearray();events=[]
        for _ in range(2048):
            a,b=recv_exact(self.sock,2);opcode=a&15;n=b&127
            assert not b&128 and not a&64
            if n==126:n=struct.unpack('!H',recv_exact(self.sock,2))[0]
            elif n==127:n=struct.unpack('!Q',recv_exact(self.sock,8))[0]
            assert n<=2*1024*1024
            part=recv_exact(self.sock,n)
            if opcode==8:raise EOFError('ws_close')
            if opcode==9:self.frame(10,part);continue
            if opcode==10:continue
            content.extend(part);assert len(content)<=2*1024*1024
            if not a&128:continue
            event=json.loads(content);content.clear();count+=1;kind=event.get('type');events.append(event)
            if kind=='response.output_text.delta':text+=event.get('delta','')
            if kind in {'response.completed','response.failed','response.incomplete','error'}:
                response=event.get('response',{});error=event.get('error',response.get('error',{})) or {}
                facts={'completed':kind=='response.completed','exact_reply':text.strip()==marker,'downstream_decoded_bytes':len(raw),'elapsed_seconds':round(time.monotonic()-start,3),'event_count':count,'error_code':safe_code(error.get('code'))}
                return facts,response,events
        raise RuntimeError('event_limit')
    def close(self): self.sock.close()
