"""Request signing for the iphone-use device runner, for the dev scripts.

The runner refuses any request not signed with its per-launch token (wire format:
crates/core/src/runner_auth.rs). The token is read from, in order:
  IPU_RUNNER_TOKEN, the file named by IPU_RUNNER_TOKEN_FILE, or the instance state dir's
  runner-token (~/.iphone-use/runner-token; ~/.iphone-use/instances/<name>/runner-token when
  IPHONE_USE_INSTANCE is set; IPHONE_USE_STATE_DIR overrides).
Without one, requests go out unsigned (only a runner older than request signing answers them).
"""

import hashlib
import hmac
import os
import re
import secrets
import time
import urllib.parse

# Installs made before the rename set PHONE_REMOTE_*; read them as IPHONE_USE_*
# (the new name wins).
for _key, _value in list(os.environ.items()):
    if _key.startswith("PHONE_REMOTE_") and len(_key) > len("PHONE_REMOTE_"):
        os.environ.setdefault("IPHONE_USE_" + _key[len("PHONE_REMOTE_"):], _value)

SCHEME = "IPU-HMAC-SHA256"
_TOKEN = re.compile(r"^[0-9a-f]{32,128}$")


def _token_file():
    if os.environ.get("IPU_RUNNER_TOKEN_FILE"):
        return os.environ["IPU_RUNNER_TOKEN_FILE"]
    state = os.environ.get("IPHONE_USE_STATE_DIR")
    if not state:
        state = os.path.expanduser("~/.iphone-use")
        name = os.environ.get("IPHONE_USE_INSTANCE", "")
        if name and name != "default":
            state = os.path.join(state, "instances", name)
    return os.path.join(state, "runner-token")


def token():
    value = os.environ.get("IPU_RUNNER_TOKEN", "").strip()
    if not value:
        try:
            with open(_token_file()) as handle:
                value = handle.read().strip()
        except OSError:
            return None
    return value if _TOKEN.match(value) else None


def authorization(tok, method, target, body=b"", ts=None, nonce=None):
    ts = int(time.time()) if ts is None else ts
    nonce = secrets.token_hex(16) if nonce is None else nonce
    message = "\n".join([
        "ipu-runner-v1", method.upper(), target, str(ts), nonce, hashlib.sha256(body or b"").hexdigest(),
    ])
    sig = hmac.new(tok.encode(), message.encode(), hashlib.sha256).hexdigest()
    return f"{SCHEME} ts={ts}, nonce={nonce}, sig={sig}"


def sign(request):
    """Adds the Authorization header to a urllib.request.Request (no-op without a token)."""
    tok = token()
    if tok is None:
        return request
    parts = urllib.parse.urlsplit(request.full_url)
    target = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    request.add_header("Authorization", authorization(tok, request.get_method(), target, request.data or b""))
    return request


if __name__ == "__main__":
    # Known answer shared with crates/core/src/runner_auth.rs and runner/unit-check/main.swift.
    expected = "d45cade8fef49833329384ea5669106fce22a98fcc182db1ff3dc0eb946e6e11"
    got = authorization("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff", "post",
                        "/session/S/actions?x=1", b'{"a":1}', 1700000000, "0123456789abcdef0123456789abcdef")
    assert got.endswith("sig=" + expected), got
    print("runner_auth: known answer OK")
