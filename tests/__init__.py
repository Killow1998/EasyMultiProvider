import atexit
import os
import shutil
import tempfile

from cryptography.fernet import Fernet


os.environ.setdefault("EASY_MULTI_PROVIDER_MASTER_KEY", Fernet.generate_key().decode("ascii"))

# Tests must never reach the developer's real Codex login: code that falls
# back to the default CODEX_HOME (e.g. AppState.codex_home) would otherwise
# read and overwrite ~/.codex/auth.json.
_ISOLATED_CODEX_HOME = tempfile.mkdtemp(prefix="emp-tests-codex-home-")
atexit.register(shutil.rmtree, _ISOLATED_CODEX_HOME, True)
os.environ["CODEX_HOME"] = _ISOLATED_CODEX_HOME
