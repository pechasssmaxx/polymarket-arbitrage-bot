#!/usr/bin/env python3
"""
CONVERT via Polymarket Relayer with automatic retry.

This script converts NO tokens to YES moonbags + USDC via Polymarket Relayer.
Relayer executes transactions gasless (no gas fees required).

CONVERT MECHANICS:
==================
In Polymarket neg-risk markets:

1. Each market has N outcomes (e.g., 38 ranges for "how many tweets")
2. Each outcome has YES and NO tokens
3. Exactly ONE outcome will be YES (others will be NO)

CONVERT allows:
- Take K NO tokens (on different outcomes)
- Get K YES tokens + (K-1) USDC back

Why this works:
- Of K outcomes, one will be YES (win $1), rest K-1 will be NO (win $0)
- But you have NO tokens on all K outcomes
- So you're guaranteed to get (K-1) x $1 = $(K-1) USDC
- Plus K YES tokens as "moonbags" (one will win $1)

EXAMPLE:
========
- Bought 10 NO tokens on 15 outcomes (K=15)
- CONVERT gives: 15 YES tokens + 14 USDC
- Spent: ~$140 buying NO (if price ~93c each)
- Got: $140 USDC + 15 YES moonbags
- Profit: one YES will win $10, other 14 = $0
- Total: +$10 minus fees and slippage

INDEX_SET:
==========
index_set is a bitmap indicating WHICH outcomes to convert.

Each outcome has a convert_index (0, 1, 2, ... N-1).
To include an outcome in convert, set its bit position:

    index_set = (1 << convert_index_1) | (1 << convert_index_2) | ...

Example for outcomes with indices [22, 23, 24, 25, 26]:
    index_set = (1<<22) | (1<<23) | (1<<24) | (1<<25) | (1<<26)
    index_set = 4194304 + 8388608 + 16777216 + 33554432 + 67108864
    index_set = 130023424

K (number of outcomes) = number of 1-bits in index_set:
    K = bin(index_set).count('1')

IMPORTANT: index_set must include ONLY outcomes for which you have NO tokens!

Usage:
    python convert_retry.py <market_id> <index_set> <amount> [max_retries]

Arguments:
    market_id   - Neg risk market ID (bytes32 hex)
    index_set   - Bitmap of outcomes to convert (decimal or hex)
    amount      - Amount in raw units (6 decimals: 1000000 = 1.0 share)
    max_retries - Maximum attempts (default: 20)

Environment:
    POLYGON_PRIVATE_KEY     - EOA private key (Safe owner)
    POLY_BUILDER_API_KEY    - Builder API key
    POLY_BUILDER_SECRET     - Builder API secret (base64)
    POLY_BUILDER_PASSPHRASE - Builder API passphrase

Output (JSON on last line):
    {"success": true, "tx_hash": "0x...", "k": 15, "usdc_received": 140.0}
    {"success": false, "error": "..."}
"""

import os
import sys
import json
import time
from eth_abi import encode
from eth_utils import keccak

from py_builder_relayer_client.client import RelayClient
from py_builder_relayer_client.models import OperationType, SafeTransaction
from py_builder_signing_sdk.config import BuilderConfig, BuilderApiKeyCreds

# Constants
NEG_RISK_ADAPTER = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296"
RELAYER_URL = "https://relayer-v2.polymarket.com/"
CHAIN_ID = 137  # Polygon mainnet


def log(msg: str):
    """Print timestamped log to stderr (won't interfere with JSON output)"""
    import datetime
    ts = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{ts}] {msg}", file=sys.stderr)


def encode_convert_positions(market_id: bytes, index_set: int, amount: int) -> str:
    """
    Encode calldata for convertPositions on NegRiskAdapter.

    Function signature: convertPositions(bytes32 marketId, uint256 indexSet, uint256 amount)
    """
    # First 4 bytes = keccak256 hash of function signature
    selector = keccak(text="convertPositions(bytes32,uint256,uint256)")[:4]
    # Remaining bytes = ABI-encoded parameters
    params = encode(['bytes32', 'uint256', 'uint256'], [market_id, index_set, amount])
    return "0x" + (selector + params).hex()


def fix_base64_padding(secret: str) -> str:
    """
    Fix base64 string padding.
    Base64 requires length divisible by 4, missing chars are filled with '='.
    """
    padding_needed = (4 - len(secret) % 4) % 4
    if padding_needed:
        return secret + '=' * padding_needed
    return secret


def create_builder_config() -> BuilderConfig:
    """Create Builder API config from environment variables."""
    api_key = os.getenv('POLY_BUILDER_API_KEY')
    secret = os.getenv('POLY_BUILDER_SECRET')
    passphrase = os.getenv('POLY_BUILDER_PASSPHRASE')

    if not all([api_key, secret, passphrase]):
        raise ValueError("Missing Builder credentials in environment")

    # Fix base64 padding if needed
    secret = fix_base64_padding(secret)

    return BuilderConfig(local_builder_creds=BuilderApiKeyCreds(
        key=api_key,
        secret=secret,
        passphrase=passphrase
    ))


def convert_with_retry(market_id_hex: str, index_set: int, amount: int, max_retries: int = 20) -> dict:
    """
    Execute CONVERT with automatic retry on timeout.

    Args:
        market_id_hex: Hex string neg risk market ID
        index_set: Bitmap of outcomes to convert
        amount: Amount in raw units (6 decimals)
        max_retries: Maximum number of retry attempts

    Returns:
        dict with success/failure info
    """
    # Parse market_id
    market_id_clean = market_id_hex.replace("0x", "")
    market_id_bytes = bytes.fromhex(market_id_clean)

    # Count K from index_set
    k = bin(index_set).count('1')

    log("=" * 60)
    log("CONVERT via Polymarket Relayer")
    log("=" * 60)
    log(f"Market ID: 0x{market_id_clean[:16]}...")
    log(f"Index Set: {index_set}")
    log(f"K (outcomes): {k}")
    log(f"Amount: {amount / 1e6:.6f} shares")
    log(f"Expected USDC: ${(k - 1) * amount / 1e6:.2f}")
    log(f"Expected YES moonbags: {k} x {amount / 1e6:.2f} shares")
    log("=" * 60)

    # Encode calldata
    calldata = encode_convert_positions(market_id_bytes, index_set, amount)
    log(f"Calldata: {calldata[:50]}...")

    # Create config
    builder_config = create_builder_config()
    private_key = os.getenv('POLYGON_PRIVATE_KEY')

    if not private_key:
        raise ValueError("POLYGON_PRIVATE_KEY not set")

    # Create Safe transaction
    txn = SafeTransaction(
        to=NEG_RISK_ADAPTER,
        operation=OperationType.Call,
        data=calldata,
        value="0"
    )

    log(f"\nStarting retry loop (max {max_retries} attempts)...\n")

    for attempt in range(1, max_retries + 1):
        try:
            log(f"[Attempt {attempt}/{max_retries}] Connecting to relayer...")

            # Create client (new each time for fresh nonce)
            client = RelayClient(RELAYER_URL, CHAIN_ID, private_key, builder_config)

            # Verify Safe
            safe_address = client.get_expected_safe()
            log(f"[Attempt {attempt}] Safe: {safe_address}")

            if not client.get_deployed(safe_address):
                raise Exception(f"Safe {safe_address} is not deployed")

            # Submit transaction
            log(f"[Attempt {attempt}] Submitting to relayer...")
            result = client.execute([txn])

            tx_hash = result.transaction_hash
            tx_id = result.transaction_id
            log(f"[Attempt {attempt}] Submitted: {tx_hash}")
            log(f"[Attempt {attempt}] TX ID: {tx_id}")

            # Wait for confirmation (timeout ~2 min built into client)
            log(f"[Attempt {attempt}] Waiting for confirmation...")
            final = result.wait()

            if final:
                state = final.get("state", "UNKNOWN")

                if state in ["STATE_CONFIRMED", "STATE_MINED"]:
                    # SUCCESS!
                    tx_hash = final.get("transactionHash", tx_hash)

                    log("\n" + "=" * 60)
                    log("SUCCESS!")
                    log("=" * 60)
                    log(f"TX Hash: {tx_hash}")
                    log(f"State: {state}")
                    log(f"Polygonscan: https://polygonscan.com/tx/{tx_hash}")
                    log("=" * 60)
                    log(f"\nResult:")
                    log(f"  - Converted: {k} NO tokens")
                    log(f"  - Received: ${(k - 1) * amount / 1e6:.2f} USDC")
                    log(f"  - Received: {k} YES moonbags ({amount / 1e6:.2f} shares each)")

                    return {
                        "success": True,
                        "tx_hash": tx_hash,
                        "tx_id": tx_id,
                        "state": state,
                        "k": k,
                        "amount": amount,
                        "usdc_received": (k - 1) * amount / 1e6
                    }
                else:
                    log(f"[Attempt {attempt}] Unexpected state: {state}")
            else:
                log(f"[Attempt {attempt}] Timeout - no confirmation received")

        except Exception as e:
            error_msg = str(e)
            log(f"[Attempt {attempt}] Error: {error_msg[:100]}")

            # Handle specific errors
            if "GS026" in error_msg:
                log("  -> Nonce mismatch, waiting 5s before retry...")
                time.sleep(5)
            elif "GS013" in error_msg:
                log("  -> Signature verification failed")
            elif "execution reverted" in error_msg.lower():
                log("  -> Contract execution reverted - check token balances")
                # Serious error, don't retry
                return {"success": False, "error": error_msg}

        # Pause between attempts
        if attempt < max_retries:
            log(f"[Attempt {attempt}] Waiting 3s before retry...\n")
            time.sleep(3)

    log(f"\nFailed after {max_retries} attempts")
    return {"success": False, "error": "Max retries exceeded"}


def calculate_index_set(convert_indices: list) -> int:
    """
    Helper function to calculate index_set from list of convert indices.

    Args:
        convert_indices: List of convert_index values for outcomes

    Returns:
        index_set as integer

    Example:
        >>> calculate_index_set([22, 23, 24, 25, 26])
        130023424
    """
    index_set = 0
    for idx in convert_indices:
        index_set |= (1 << idx)
    return index_set


def main():
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        print("\nExample usage:", file=sys.stderr)
        print("  python convert_retry.py 0x59dd0249b3baa26e... 240513974272 10000000", file=sys.stderr)
        print("\nHelp calculating index_set:", file=sys.stderr)
        print("  If you have outcomes with convert_index: 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 36, 37", file=sys.stderr)
        print("  Then index_set = sum(1 << i for i in [22,23,24,25,26,27,28,29,30,31,32,33,34,36,37])", file=sys.stderr)
        print("  = 240513974272", file=sys.stderr)
        # Output JSON error for Rust to parse
        print(json.dumps({"success": False, "error": "Missing arguments"}))
        sys.exit(1)

    market_id = sys.argv[1]

    # Support hex or decimal for index_set
    index_set_str = sys.argv[2]
    if index_set_str.startswith("0x"):
        index_set = int(index_set_str, 16)
    else:
        index_set = int(index_set_str)

    amount = int(sys.argv[3])
    max_retries = int(sys.argv[4]) if len(sys.argv) > 4 else 20

    try:
        result = convert_with_retry(market_id, index_set, amount, max_retries)
    except Exception as e:
        result = {"success": False, "error": str(e)}

    # Output JSON on last line for Rust to parse
    print(json.dumps(result))

    if result.get("success"):
        sys.exit(0)
    else:
        sys.exit(1)


if __name__ == "__main__":
    main()
