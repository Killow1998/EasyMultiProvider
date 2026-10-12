"""Read-only release metadata for previewing updates against older EMP backends."""
import json
import re
import threading
import time
import urllib.request
from urllib.parse import urlsplit

PUBLIC_RELEASE = 'https://api.github.com/repos/Killow1998/EasyMultiProvider/releases/latest'

class ReleaseNotes:
    def __init__(self, endpoint=PUBLIC_RELEASE):
        self.endpoint=endpoint
        self.lock=threading.Lock()
        self.cached=None
        self.expires=0

    def read(self):
        with self.lock:
            if self.cached and time.monotonic()<self.expires:
                return self.cached
            request=urllib.request.Request(self.endpoint,headers={'Accept':'application/vnd.github+json','User-Agent':'EMP-shadow-preview'})
            opener=urllib.request.build_opener(urllib.request.ProxyHandler({})) if urlsplit(self.endpoint).hostname == '127.0.0.1' else urllib.request.build_opener()
            with opener.open(request,timeout=10) as response:
                raw=response.read(1024*1024+1)
            if len(raw)>1024*1024:
                raise ValueError('Release metadata too large')
            release=json.loads(raw)
            if release.get('draft') or release.get('prerelease') or not re.fullmatch(r'v?\d+\.\d+\.\d+',release.get('tag_name','')):
                raise ValueError('Invalid stable release')
            body=release.get('body') or ''
            if not isinstance(body,str):
                raise ValueError('Invalid release notes')
            self.cached={'latest_version':release['tag_name'].removeprefix('v'),'release_notes':body[:65536]}
            self.expires=time.monotonic()+900
            return self.cached
