#!/usr/bin/env python3
"""
CONVERT via Polymarket Relayer (gasless)

Usage: python convert_relayer.py <market_id> <index_set> <amount>

Arguments:
  market_id   - Neg risk market ID (hex, with or without 0x prefix)
  index_set   - Bitmap of outcomes to convert (e.g., 3 for K=2)
  amount      - Amount in raw units (6 decimals, e.g., 1000000 = 1.0)

Environment variables required:
  POLYGON_PRIVATE_KEY     - EOA private key (owner of the Safe)
  POLYGON_RPC_URL         - Polygon RPC URL (for nonce lookup fallback)
  POLY_BUILDER_API_KEY    - Builder API key
  POLY_BUILDER_SECRET     - Builder API secret (must be valid base64!)
  POLY_BUILDER_PASSPHRASE - Builder API passphrase

Optional:
  RELAYER_URL - Relayer endpoint (default: https://relayer-v2.polymarket.com/)

IMPORTANT: POLY_BUILDER_SECRET must be valid base64 with correct padding.
If you get "Incorrect padding" error, the secret length must be divisible by 4.
Add '=' characters to pad if needed.
"""

import os
import sys
import json
import logging
from eth_abi import encode
from eth_utils import keccak

from py_builder_relayer_client.client import RelayClient
from py_builder_relayer_client.models import OperationType, SafeTransaction
from py_builder_signing_sdk.config import BuilderConfig, BuilderApiKeyCreds

# Contract addresses (Polygon mainnet)
NEG_RISK_ADAPTER = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296"

# Configure logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s [%(levelname)s] %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)
logger = logging.getLogger(__name__)


def encode_convert_positions(market_id: bytes, index_set: int, amount: int) -> str:
    """Encode convertPositions(bytes32,uint256,uint256) calldata"""
    selector = keccak(text="convertPositions(bytes32,uint256,uint256)")[:4]
    params = encode(['bytes32', 'uint256', 'uint256'], [market_id, index_set, amount])
    return "0x" + (selector + params).hex()


def validate_base64_secret(secret: str) -> str:
    """Validate and fix base64 padding if needed"""
    import base64

    # Try to decode as-is
    try:
        base64.urlsafe_b64decode(secret)
        return secret
    except Exception:
        pass

    # Try adding padding
    padding_needed = (4 - len(secret) % 4) % 4
    if padding_needed > 0:
        padded = secret + '=' * padding_needed
        try:
            base64.urlsafe_b64decode(padded)
            logger.warning(f"Fixed base64 padding: added {padding_needed} '=' chars")
            return padded
        except Exception:
            pass

    raise ValueError(f"Invalid base64 secret (length={len(secret)}). Check POLY_BUILDER_SECRET.")


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        sys.exit(1)

    # Parse args
    market_id_hex = sys.argv[1]
    index_set = int(sys.argv[2])
    amount = int(sys.argv[3])

    # Calculate K from index_set
    k = bin(index_set).count('1')

    logger.info(f"CONVERT: market={market_id_hex[:16]}..., K={k}, index_set={index_set}, amount={amount/1e6:.6f}")

    # Config from env
    relayer_url = os.getenv("RELAYER_URL", "https://relayer-v2.polymarket.com/")
    chain_id = 137  # Polygon mainnet
    pk = os.getenv("POLYGON_PRIVATE_KEY")

    if not pk:
        logger.error("POLYGON_PRIVATE_KEY not set")
        print(json.dumps({"success": False, "error": "POLYGON_PRIVATE_KEY not set"}))
        sys.exit(1)

    # Builder credentials
    builder_key = os.getenv("POLY_BUILDER_API_KEY")
    builder_secret = os.getenv("POLY_BUILDER_SECRET")
    builder_passphrase = os.getenv("POLY_BUILDER_PASSPHRASE")

    if not all([builder_key, builder_secret, builder_passphrase]):
        logger.error("Builder credentials not set (POLY_BUILDER_API_KEY, POLY_BUILDER_SECRET, POLY_BUILDER_PASSPHRASE)")
        print(json.dumps({"success": False, "error": "Builder credentials not set"}))
        sys.exit(1)

    # Validate and fix base64 secret
    try:
        builder_secret = validate_base64_secret(builder_secret)
    except ValueError as e:
        logger.error(str(e))
        print(json.dumps({"success": False, "error": str(e)}))
        sys.exit(1)

    builder_config = BuilderConfig(local_builder_creds=BuilderApiKeyCreds(
        key=builder_key,
        secret=builder_secret,
        passphrase=builder_passphrase
    ))

    # Build CONVERT transaction calldata
    market_id_clean = market_id_hex.replace("0x", "")
    market_id_bytes = bytes.fromhex(market_id_clean)
    calldata = encode_convert_positions(market_id_bytes, index_set, amount)

    logger.info(f"Calldata: {calldata[:50]}...")

    # Create SafeTransaction
    txn = SafeTransaction(
        to=NEG_RISK_ADAPTER,
        operation=OperationType.Call,
        data=calldata,
        value="0"
    )

    try:
        # Create client
        client = RelayClient(relayer_url, chain_id, pk, builder_config)

        # Verify Safe deployment
        safe_address = client.get_expected_safe()
        if not client.get_deployed(safe_address):
            raise Exception(f"Safe {safe_address} is not deployed")

        logger.info(f"Safe: {safe_address}")
        logger.info("Submitting CONVERT to relayer...")

        # Execute using simple method (handles nonce internally)
        result = client.execute([txn])

        tx_hash = result.transaction_hash
        tx_id = result.transaction_id

        logger.info(f"Transaction submitted: id={tx_id}, hash={tx_hash}")
        logger.info("Waiting for confirmation...")

        # Wait for confirmation
        final = result.wait()

        if final is None:
            logger.error("Transaction failed or timed out")
            print(json.dumps({
                "success": False,
                "tx_hash": tx_hash,
                "tx_id": tx_id,
                "error": "Transaction failed or timed out"
            }))
            sys.exit(1)

        state = final.get("state", "UNKNOWN")
        tx_hash = final.get("transactionHash", tx_hash)

        # Check for success states
        success = state in ["STATE_CONFIRMED", "STATE_MINED"]

        logger.info(f"Transaction complete: hash={tx_hash}, state={state}, success={success}")

        # Output JSON for Rust to parse (last line)
        print(json.dumps({
            "success": success,
            "tx_hash": tx_hash,
            "tx_id": tx_id,
            "state": state,
            "amount": amount,
            "k": k
        }))

        sys.exit(0 if success else 1)

    except Exception as e:
        error_msg = str(e)
        logger.error(f"Error: {error_msg}")

        # Provide helpful error messages
        if "Incorrect padding" in error_msg:
            error_msg = "Invalid POLY_BUILDER_SECRET base64 encoding. Length must be divisible by 4."
        elif "GS026" in error_msg:
            error_msg = "Invalid nonce - Safe nonce mismatch. Try again."
        elif "GS013" in error_msg:
            error_msg = "Signature verification failed. Check credentials."

        print(json.dumps({
            "success": False,
            "error": error_msg
        }))
        sys.exit(1)


if __name__ == "__main__":
    main()
