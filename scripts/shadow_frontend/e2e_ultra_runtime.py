"""Run an installed Codex against E2E-produced catalogs and a loopback Responses fixture."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Responses(BaseHTTPRequestHandler):
    efforts = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd':
            from compression import zstd
            body = zstd.decompress(body)
        body = json.loads(body)
        self.efforts.append(body.get('reasoning', {}).get('effort'))
        item = {'id':'msg-fixture','type':'message','role':'assistant','status':'completed',
            'content':[{'type':'output_text','text':'fixture complete','annotations':[]}]}
        response = {'id':'resp-fixture','object':'response','model':body['model'],
            'status':'completed','output':[item], 'usage':{'input_tokens':10,'output_tokens':2,'total_tokens':12}}
        events = [{'type':'response.created','response':{**response,'status':'in_progress','output':[]}},
            {'type':'response.output_item.added','output_index':0,'item':{**item,'status':'in_progress','content':[]}},
            {'type':'response.output_text.delta','item_id':item['id'],'output_index':0,'content_index':0,'delta':'fixture complete'},
            {'type':'response.output_item.done','output_index':0,'item':item},
            {'type':'response.completed','response':response}]
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.send_header('Connection','close')
        self.end_headers()
        self.wfile.write(''.join('data: '+json.dumps(event)+'\n\n' for event in events).encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--catalogs', type=Path, required=True, help='Output of e2e_ultra_effort.py')
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix='emp-ultra-runtime-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        server = ThreadingHTTPServer(('127.0.0.1',0),Responses)
        stack.callback(server.server_close)
        threading.Thread(target=server.serve_forever,daemon=True).start()
        stack.callback(server.shutdown)
        for catalog, selected, expected in [('low','ultra','low'),('default','ultra','xhigh'),('low','high','high')]:
            home = root/f'{catalog}-{selected}'
            home.mkdir()
            env = {key:value for key,value in os.environ.items() if not any(word in key.upper() for word in ['TOKEN','KEY','SECRET','AUTH']) and not key.startswith(('CODEX_','OPENAI_','ANTHROPIC_','EASY_MULTI_PROVIDER'))}
            env.update(HOME=str(home),CODEX_HOME=str(home),XDG_CONFIG_HOME=str(home),
                HTTP_PROXY='http://127.0.0.1:9',HTTPS_PROXY='http://127.0.0.1:9',ALL_PROXY='http://127.0.0.1:9',NO_PROXY='127.0.0.1,localhost')
            (home/'config.toml').write_text('model="gpt-ultra"\nmodel_provider="fixture"\napproval_policy="never"\n'
                'sandbox_mode="read-only"\nweb_search="disabled"\ncli_auth_credentials_store="file"\n'
                f'model_reasoning_effort={json.dumps(selected)}\n'
                f'model_catalog_json={json.dumps(str(args.catalogs.resolve()/f"catalog-{catalog}.json"))}\n'
                '[features]\nplugins=false\nmemories=false\nasync_compaction=false\n'
                '[model_providers.fixture]\nname="OpenAI"\nwire_api="responses"\n'
                f'base_url="http://127.0.0.1:{server.server_port}/v1"\nrequires_openai_auth=false\n'
                'supports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n')
            Responses.efforts = []
            result = subprocess.run([str(args.codex.resolve()),'exec','--skip-git-repo-check','Only say fixture complete. Do not use tools.'],
                cwd=home,env=env,capture_output=True,text=True,timeout=60)
            assert result.returncode == 0, result.stderr
            assert Responses.efforts == [expected], {'selected':selected,'efforts':Responses.efforts,'stderr':result.stderr}
            assert 'fixture complete' in result.stdout, result.stdout
            results.append({'catalog':catalog,'selection':selected,'observed_effort':expected,'completed':True})
    report = {'real_model_calls':0,'cases':results}
    (args.catalogs/'runtime-result.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
