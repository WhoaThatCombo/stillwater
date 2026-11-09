#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Sign a Stillwater setup for the auto-updater, or check a signature.

    python tools/release/sign.py Stillwater-Setup-0.8.3.exe
        writes Stillwater-Setup-0.8.3.exe.sig (64 raw bytes, Ed25519 over the whole file).
        Upload it to the GitHub release next to the .exe and its .sha256.

    python tools/release/sign.py --verify Stillwater-Setup-0.8.3.exe [--sig other.sig]
        checks the signature against the public key built into Stillwater
        (UPDATE_PUBLIC_KEY in ports/servoshell/desktop/updater.rs).

The private key is read from --key, else STILLWATER_SIGNING_KEY, else
M:\\stillwater-signing\\stillwater-release.key. It must never be put in this repository.
Needs the `cryptography` package.
"""

import argparse
import os
import re
import sys
from pathlib import Path

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

DEFAULT_KEY = Path(r"M:\stillwater-signing\stillwater-release.key")
UPDATER_RS = Path(__file__).resolve().parents[2] / "ports" / "servoshell" / "desktop" / "updater.rs"


def embedded_public_key() -> Ed25519PublicKey:
    """The UPDATE_PUBLIC_KEY array from updater.rs, so there's only one copy of it."""
    source = UPDATER_RS.read_text(encoding="utf-8")
    match = re.search(r"const UPDATE_PUBLIC_KEY: \[u8; 32\] = \[([^\]]*)\]", source)
    if not match:
        sys.exit(f"couldn't find UPDATE_PUBLIC_KEY in {UPDATER_RS}")
    key = bytes(int(byte, 16) for byte in re.findall(r"0x([0-9a-fA-F]{2})", match.group(1)))
    if len(key) != 32:
        sys.exit(f"UPDATE_PUBLIC_KEY in {UPDATER_RS} isn't 32 bytes")
    return Ed25519PublicKey.from_public_bytes(key)


def raw_public_bytes(key: Ed25519PublicKey) -> bytes:
    return key.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)


def load_private_key(path: Path) -> Ed25519PrivateKey:
    key = serialization.load_pem_private_key(path.read_bytes(), password=None)
    if not isinstance(key, Ed25519PrivateKey):
        sys.exit(f"{path} isn't an Ed25519 private key")
    return key


def main() -> int:
    parser = argparse.ArgumentParser(description="Sign or verify a Stillwater setup.")
    parser.add_argument("installer", type=Path, help="the Stillwater-Setup-<version>.exe")
    parser.add_argument("--verify", action="store_true", help="check the signature instead of signing")
    parser.add_argument("--sig", type=Path, help="signature file (default: <installer>.sig)")
    parser.add_argument("--key", type=Path, help="private key (default: $STILLWATER_SIGNING_KEY or M:)")
    args = parser.parse_args()

    sig_path = args.sig or args.installer.with_name(args.installer.name + ".sig")
    data = args.installer.read_bytes()
    public_key = embedded_public_key()

    if args.verify:
        signature = sig_path.read_bytes()
        try:
            if len(signature) != 64:
                raise InvalidSignature
            public_key.verify(signature, data)
        except InvalidSignature:
            print(f"BAD signature: {sig_path} doesn't match {args.installer}", file=sys.stderr)
            return 1
        print(f"OK: {args.installer} is signed with the Stillwater release key")
        return 0

    key_path = args.key or Path(os.environ.get("STILLWATER_SIGNING_KEY") or DEFAULT_KEY)
    private_key = load_private_key(key_path)
    # Signing with a key the browser doesn't trust would only make updates fail later.
    if raw_public_bytes(private_key.public_key()) != raw_public_bytes(public_key):
        sys.exit(f"{key_path} doesn't match UPDATE_PUBLIC_KEY in {UPDATER_RS}")
    signature = private_key.sign(data)
    public_key.verify(signature, data)
    sig_path.write_bytes(signature)
    print(f"wrote {sig_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
