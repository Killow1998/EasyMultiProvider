import os

from cryptography.fernet import Fernet


os.environ.setdefault("EASY_MULTI_PROVIDER_MASTER_KEY", Fernet.generate_key().decode("ascii"))
