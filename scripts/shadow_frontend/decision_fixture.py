"""Loopback decision API and catalogs shared by Shadow and browser acceptance."""
import json
from http.server import BaseHTTPRequestHandler
from urllib.parse import parse_qs, urlsplit


class DecisionUpstream(BaseHTTPRequestHandler):
    calls = []
    catalogs = []
    catalog_status = 200
    empty_catalog = False
    extra_models = []

    def log_message(self, *args):
        pass

    def reply(self, status, payload):
        encoded = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self):
        self.catalogs.append((self.path, {name.lower():value for name,value in self.headers.items()}))
        if self.catalog_status != 200:
            self.reply(self.catalog_status, {'error':{'message':'Synthetic catalog failure'}})
            return
        name = urlsplit(self.path).path.split('/')[1]
        if name == 'typesafe':
            payload = {'models':[] if self.empty_catalog else [
                {'name':'jev-latest','description':'Latest decision model','release_date':'2026-10-30'},
                {'name':'jev-1.14.0','description':'Pinned decision model','release_date':'2026-10-30'}]}
        elif name == 'openai':
            payload = {'data':[] if self.empty_catalog else [{'id':'gpt-6-luna','name':'GPT-6 Luna'}, {'id':'gpt-6.1-sol'}]}
        else:
            items = [{'id':'typesafe/jev-router','name':'TypeSafe: Jev Router','architecture':{'output_modalities':['text']}}]
            if parse_qs(urlsplit(self.path).query).get('output_modalities') == ['decisions']:
                items += [{'id':model_id,'name':label,'architecture':{'output_modalities':['decisions']}} for model_id,label in [
                    ('typesafe/jev-1.14','TypeSafe: Jev 1.14'), ('~typesafe/jev-latest','TypeSafe: Jev Latest'),
                    ('openai/gpt-6-luna-decisions','OpenAI: GPT-6 Luna Decisions'), *self.extra_models]]
            payload = {'data':[] if self.empty_catalog else items}
        self.reply(200, payload)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.calls.append((self.path, {name.lower():value for name,value in self.headers.items()}, body))
        self.reply(200, {'model':body['model'], 'answers':[] if '/openai/' in self.path else {},
                         'usage':{'input_tokens':7, 'output_tokens':2}})
