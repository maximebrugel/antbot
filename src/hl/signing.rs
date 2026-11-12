//! L1 action signing for the Hyperliquid `/exchange` endpoint.
//!
//! Port of `hyperliquid/utils/signing.py` from the official Python SDK
//! (hyperliquid-python-sdk 0.24.0, commit 2fdb18f9):
//!
//! ```text
//! data   = msgpack(action) ++ nonce(u64 BE)
//!          ++ (0x00 | 0x01 ++ vault_address[20])
//!          ++ [0x00 ++ expires_after(u64 BE)]          (only when set)
//! hash   = keccak256(data)                              (= connectionId)
//! agent  = { source: "a" (mainnet) | "b" (testnet), connectionId: hash }
//! sig    = secp256k1 over EIP-712 Agent(string source,bytes32 connectionId),
//!          domain { name "Exchange", version "1", chainId 1337, verifyingContract 0x0 }
//! ```
//!
//! The action is serialized with `rmp_serde::to_vec_named` so structs become
//! msgpack maps whose keys follow the struct field order. Every wire struct in
//! [`crate::hl::actions`] therefore declares its fields in the exact order the
//! Python SDK builds its dicts. Tests reproduce the SDK's own test vectors and
//! extra vectors generated with the SDK for HIP-3 asset 110138 (xyz:ANT).

use std::fmt;

use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

pub type H256 = [u8; 32];

/// 20-byte EVM address.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address(pub [u8; 20]);

impl Address {
    /// Lowercase `0x`-prefixed hex (the form the API and SDK use).
    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::str::FromStr for Address {
    type Err = SigningError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        let bytes = hex::decode(h).map_err(|_| SigningError::BadAddress)?;
        let arr: [u8; 20] = bytes.try_into().map_err(|_| SigningError::BadAddress)?;
        Ok(Address(arr))
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SigningError {
    #[error("malformed private key")]
    BadKey,
    #[error("malformed address")]
    BadAddress,
    #[error("msgpack encoding failed: {0}")]
    Encode(String),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("float_to_wire causes rounding: {0}")]
    WireRounding(f64),
    #[error("float_to_int causes rounding: {0}")]
    IntRounding(f64),
    #[error("signature recovery failed")]
    Recover,
}

pub fn keccak256(data: &[u8]) -> H256 {
    Keccak256::digest(data).into()
}

/// A local secp256k1 wallet. `Debug` never prints the key.
#[derive(Clone)]
pub struct Wallet {
    key: SigningKey,
    address: Address,
}

impl fmt::Debug for Wallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Wallet {
    /// Parse a `0x`-prefixed (or bare) 32-byte hex private key.
    pub fn from_hex(key: &str) -> Result<Self, SigningError> {
        let h = key
            .trim()
            .strip_prefix("0x")
            .or_else(|| key.trim().strip_prefix("0X"))
            .unwrap_or(key.trim());
        if h.len() != 64 {
            return Err(SigningError::BadKey);
        }
        let bytes = hex::decode(h).map_err(|_| SigningError::BadKey)?;
        let key = SigningKey::from_slice(&bytes).map_err(|_| SigningError::BadKey)?;
        let address = address_of(key.verifying_key());
        Ok(Self { key, address })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// Sign a 32-byte prehash (Ethereum-style, deterministic RFC 6979, low-s).
    pub fn sign_hash(&self, hash: &H256) -> Result<Signature, SigningError> {
        let (sig, recid): (EcdsaSignature, RecoveryId) = self
            .key
            .sign_prehash_recoverable(hash)
            .map_err(|e| SigningError::Sign(e.to_string()))?;
        let bytes = sig.to_bytes();
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&bytes[..32]);
        s.copy_from_slice(&bytes[32..]);
        Ok(Signature {
            r,
            s,
            v: 27 + recid.to_byte(),
        })
    }
}

/// Ethereum address of a public key: last 20 bytes of keccak(uncompressed[1..]).
pub fn address_of(vk: &VerifyingKey) -> Address {
    let point = vk.to_encoded_point(false);
    let h = keccak256(&point.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

/// ECDSA signature in the `{r, s, v}` JSON shape the exchange expects.
///
/// `r`/`s` are serialized like the Python SDK (`eth_utils.to_hex(int)`):
/// lowercase, `0x`-prefixed, **without** leading zeros.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u8,
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signature")
            .field("r", &self.r_hex())
            .field("s", &self.s_hex())
            .field("v", &self.v)
            .finish()
    }
}

fn int_hex(b: &[u8; 32]) -> String {
    let h = hex::encode(b);
    let t = h.trim_start_matches('0');
    if t.is_empty() {
        "0x0".to_string()
    } else {
        format!("0x{t}")
    }
}

impl Signature {
    pub fn r_hex(&self) -> String {
        int_hex(&self.r)
    }
    pub fn s_hex(&self) -> String {
        int_hex(&self.s)
    }

    /// Recover the signer address for a prehash.
    pub fn recover(&self, hash: &H256) -> Result<Address, SigningError> {
        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(&self.r);
        rs[32..].copy_from_slice(&self.s);
        let sig = EcdsaSignature::from_slice(&rs).map_err(|_| SigningError::Recover)?;
        let recid = RecoveryId::from_byte(self.v.wrapping_sub(27)).ok_or(SigningError::Recover)?;
        let vk = VerifyingKey::recover_from_prehash(hash, &sig, recid)
            .map_err(|_| SigningError::Recover)?;
        Ok(address_of(&vk))
    }
}

#[derive(Serialize, Deserialize)]
struct SignatureJson {
    r: String,
    s: String,
    v: u8,
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        SignatureJson {
            r: self.r_hex(),
            s: self.s_hex(),
            v: self.v,
        }
        .serialize(ser)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let j = SignatureJson::deserialize(de)?;
        let parse = |s: &str| -> Result<[u8; 32], D::Error> {
            let h = s.strip_prefix("0x").unwrap_or(s);
            if h.len() > 64 {
                return Err(D::Error::custom("signature component too long"));
            }
            let padded = format!("{h:0>64}");
            let v = hex::decode(padded).map_err(D::Error::custom)?;
            Ok(v.try_into().expect("64 hex chars"))
        };
        Ok(Signature {
            r: parse(&j.r)?,
            s: parse(&j.s)?,
            v: j.v,
        })
    }
}

/// msgpack encoding with named struct fields (maps), as `msgpack.packb(dict)`.
pub fn msgpack_named<T: Serialize>(action: &T) -> Result<Vec<u8>, SigningError> {
    rmp_serde::to_vec_named(action).map_err(|e| SigningError::Encode(e.to_string()))
}

/// `action_hash(action, vault_address, nonce, expires_after)` from the SDK.
pub fn action_hash<T: Serialize>(
    action: &T,
    vault_address: Option<&Address>,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<H256, SigningError> {
    let mut data = msgpack_named(action)?;
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault_address {
        None => data.push(0x00),
        Some(a) => {
            data.push(0x01);
            data.extend_from_slice(&a.0);
        }
    }
    if let Some(e) = expires_after {
        data.push(0x00);
        data.extend_from_slice(&e.to_be_bytes());
    }
    Ok(keccak256(&data))
}

/// Phantom agent `source`: "a" on mainnet, "b" otherwise.
pub fn phantom_source(is_mainnet: bool) -> &'static str {
    if is_mainnet {
        "a"
    } else {
        "b"
    }
}

const EIP712_DOMAIN_TYPE: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const AGENT_TYPE: &str = "Agent(string source,bytes32 connectionId)";
/// L1 actions are always signed with chainId 1337 (domain "Exchange").
pub const L1_CHAIN_ID: u64 = 1337;

fn u256_be(v: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&v.to_be_bytes());
    out
}

/// EIP-712 domain separator of the "Exchange" domain.
pub fn exchange_domain_separator() -> H256 {
    let mut enc = Vec::with_capacity(32 * 5);
    enc.extend_from_slice(&keccak256(EIP712_DOMAIN_TYPE.as_bytes()));
    enc.extend_from_slice(&keccak256(b"Exchange"));
    enc.extend_from_slice(&keccak256(b"1"));
    enc.extend_from_slice(&u256_be(L1_CHAIN_ID));
    enc.extend_from_slice(&[0u8; 32]); // verifyingContract = 0x0
    keccak256(&enc)
}

/// EIP-712 digest of the phantom agent for `connection_id`.
pub fn l1_signing_digest(connection_id: &H256, is_mainnet: bool) -> H256 {
    let mut enc = Vec::with_capacity(32 * 3);
    enc.extend_from_slice(&keccak256(AGENT_TYPE.as_bytes()));
    enc.extend_from_slice(&keccak256(phantom_source(is_mainnet).as_bytes()));
    enc.extend_from_slice(connection_id);
    let struct_hash = keccak256(&enc);
    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(&[0x19, 0x01]);
    msg.extend_from_slice(&exchange_domain_separator());
    msg.extend_from_slice(&struct_hash);
    keccak256(&msg)
}

/// `sign_l1_action(wallet, action, active_pool, nonce, expires_after, is_mainnet)`.
pub fn sign_l1_action<T: Serialize>(
    wallet: &Wallet,
    action: &T,
    vault_address: Option<&Address>,
    nonce: u64,
    expires_after: Option<u64>,
    is_mainnet: bool,
) -> Result<Signature, SigningError> {
    let h = action_hash(action, vault_address, nonce, expires_after)?;
    wallet.sign_hash(&l1_signing_digest(&h, is_mainnet))
}

/// Recover the signer of an L1 action (SDK `recover_agent_or_user_from_l1_action`).
pub fn recover_l1_signer<T: Serialize>(
    action: &T,
    sig: &Signature,
    vault_address: Option<&Address>,
    nonce: u64,
    expires_after: Option<u64>,
    is_mainnet: bool,
) -> Result<Address, SigningError> {
    let h = action_hash(action, vault_address, nonce, expires_after)?;
    sig.recover(&l1_signing_digest(&h, is_mainnet))
}

/// SDK `float_to_wire`: format with 8 decimals, refuse if that rounds by
/// >= 1e-12, then strip trailing zeros (Decimal.normalize + `:f`).
///
/// One deliberate difference: the SDK returns `"-0"` for `-0.0` (its `"-0"`
/// guard never triggers because the 8-decimal string is `"-0.00000000"`);
/// we return `"0"`. Prices and sizes are never negative anyway.
pub fn float_to_wire(x: f64) -> Result<String, SigningError> {
    if !x.is_finite() {
        return Err(SigningError::WireRounding(x));
    }
    let rounded = format!("{x:.8}");
    let back: f64 = rounded.parse().map_err(|_| SigningError::WireRounding(x))?;
    if (back - x).abs() >= 1e-12 {
        return Err(SigningError::WireRounding(x));
    }
    let mut s = rounded;
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".to_string();
    }
    Ok(s)
}

/// SDK `float_to_int(x, power)`.
pub fn float_to_int(x: f64, power: u32) -> Result<i128, SigningError> {
    let with_decimals = x * 10u64.pow(power) as f64;
    let r = with_decimals.round_ties_even();
    if (r - with_decimals).abs() >= 1e-3 {
        return Err(SigningError::IntRounding(x));
    }
    Ok(r as i128)
}

/// SDK `float_to_int_for_hashing` (8 decimals).
pub fn float_to_int_for_hashing(x: f64) -> Result<i128, SigningError> {
    float_to_int(x, 8)
}

/// SDK `float_to_usd_int` (6 decimals).
pub fn float_to_usd_int(x: f64) -> Result<i128, SigningError> {
    float_to_int(x, 6)
}

#[cfg(test)]
mod tests {
    //! Vectors from the official SDK `tests/signing_test.py`
    //! (hyperliquid-python-sdk commit 2fdb18f9517675ea03695a0962bd19eece9c83f0).
    //! The order-based vectors live in `crate::hl::actions` tests; HIP-3
    //! vectors generated with the SDK live in `tests/signing_vectors.rs`.
    use super::*;

    pub const SDK_TEST_KEY: &str =
        "0x0123456789012345678901234567890123456789012345678901234567890123";

    fn wallet() -> Wallet {
        Wallet::from_hex(SDK_TEST_KEY).unwrap()
    }

    fn check(sig: Signature, r: &str, s: &str, v: u8) {
        assert_eq!(sig.r_hex(), r, "r");
        assert_eq!(sig.s_hex(), s, "s");
        assert_eq!(sig.v, v, "v");
    }

    #[derive(Serialize)]
    struct Dummy {
        #[serde(rename = "type")]
        ty: &'static str,
        num: u64,
    }

    fn dummy() -> Dummy {
        Dummy {
            ty: "dummy",
            num: float_to_int_for_hashing(1000.0).unwrap() as u64,
        }
    }

    #[test]
    fn address_of_sdk_test_key() {
        // eth_account.Account.from_key(SDK_TEST_KEY).address
        assert_eq!(
            wallet().address().to_hex(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
    }

    #[test]
    fn test_l1_action_signing_matches() {
        let w = wallet();
        let a = dummy();
        check(
            sign_l1_action(&w, &a, None, 0, None, true).unwrap(),
            "0x53749d5b30552aeb2fca34b530185976545bb22d0b3ce6f62e31be961a59298",
            "0x755c40ba9bf05223521753995abb2f73ab3229be8ec921f350cb447e384d8ed8",
            27,
        );
        check(
            sign_l1_action(&w, &a, None, 0, None, false).unwrap(),
            "0x542af61ef1f429707e3c76c5293c80d01f74ef853e34b76efffcb57e574f9510",
            "0x17b8b32f086e8cdede991f1e2c529f5dd5297cbe8128500e00cbaf766204a613",
            28,
        );
    }

    #[test]
    fn test_l1_action_signing_matches_with_vault() {
        let w = wallet();
        let a = dummy();
        let vault: Address = "0x1719884eb866cb12b2287399b15f7db5e7d775ea"
            .parse()
            .unwrap();
        check(
            sign_l1_action(&w, &a, Some(&vault), 0, None, true).unwrap(),
            "0x3c548db75e479f8012acf3000ca3a6b05606bc2ec0c29c50c515066a326239",
            "0x4d402be7396ce74fbba3795769cda45aec00dc3125a984f2a9f23177b190da2c",
            28,
        );
        check(
            sign_l1_action(&w, &a, Some(&vault), 0, None, false).unwrap(),
            "0xe281d2fb5c6e25ca01601f878e4d69c965bb598b88fac58e475dd1f5e56c362b",
            "0x7ddad27e9a238d045c035bc606349d075d5c5cd00a6cd1da23ab5c39d4ef0f60",
            27,
        );
    }

    #[test]
    fn test_create_sub_account_action() {
        #[derive(Serialize)]
        struct A {
            #[serde(rename = "type")]
            ty: &'static str,
            name: &'static str,
        }
        let a = A {
            ty: "createSubAccount",
            name: "example",
        };
        let w = wallet();
        check(
            sign_l1_action(&w, &a, None, 0, None, true).unwrap(),
            "0x51096fe3239421d16b671e192f574ae24ae14329099b6db28e479b86cdd6caa7",
            "0xb71f7d293af92d3772572afb8b102d167a7cef7473388286bc01f52a5c5b423",
            27,
        );
        check(
            sign_l1_action(&w, &a, None, 0, None, false).unwrap(),
            "0xa699e3ed5c2b89628c746d3298b5dc1cca604694c2c855da8bb8250ec8014a5b",
            "0x53f1b8153a301c72ecc655b1c315d64e1dcea3ee58921fd7507e35818fcc1584",
            28,
        );
    }

    #[test]
    fn test_sub_account_transfer_action() {
        #[derive(Serialize)]
        struct A {
            #[serde(rename = "type")]
            ty: &'static str,
            #[serde(rename = "subAccountUser")]
            sub_account_user: &'static str,
            #[serde(rename = "isDeposit")]
            is_deposit: bool,
            usd: u64,
        }
        let a = A {
            ty: "subAccountTransfer",
            sub_account_user: "0x1d9470d4b963f552e6f671a81619d395877bf409",
            is_deposit: true,
            usd: 10,
        };
        let w = wallet();
        check(
            sign_l1_action(&w, &a, None, 0, None, true).unwrap(),
            "0x43592d7c6c7d816ece2e206f174be61249d651944932b13343f4d13f306ae602",
            "0x71a926cb5c9a7c01c3359ec4c4c34c16ff8107d610994d4de0e6430e5cc0f4c9",
            28,
        );
        check(
            sign_l1_action(&w, &a, None, 0, None, false).unwrap(),
            "0xe26574013395ad55ee2f4e0575310f003c5bb3351b5425482e2969fa51543927",
            "0xefb08999196366871f919fd0e138b3a7f30ee33e678df7cfaf203e25f0a4278",
            28,
        );
    }

    #[test]
    fn test_schedule_cancel_action() {
        #[derive(Serialize)]
        struct A {
            #[serde(rename = "type")]
            ty: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            time: Option<u64>,
        }
        let w = wallet();
        let a = A {
            ty: "scheduleCancel",
            time: None,
        };
        check(
            sign_l1_action(&w, &a, None, 0, None, true).unwrap(),
            "0x6cdfb286702f5917e76cd9b3b8bf678fcc49aec194c02a73e6d4f16891195df9",
            "0x6557ac307fa05d25b8d61f21fb8a938e703b3d9bf575f6717ba21ec61261b2a0",
            27,
        );
        check(
            sign_l1_action(&w, &a, None, 0, None, false).unwrap(),
            "0xc75bb195c3f6a4e06b7d395acc20bbb224f6d23ccff7c6a26d327304e6efaeed",
            "0x342f8ede109a29f2c0723bd5efb9e9100e3bbb493f8fb5164ee3d385908233df",
            28,
        );
        let a = A {
            ty: "scheduleCancel",
            time: Some(123456789),
        };
        check(
            sign_l1_action(&w, &a, None, 0, None, true).unwrap(),
            "0x609cb20c737945d070716dcc696ba030e9976fcf5edad87afa7d877493109d55",
            "0x16c685d63b5c7a04512d73f183b3d7a00da5406ff1f8aad33f8ae2163bab758b",
            28,
        );
        check(
            sign_l1_action(&w, &a, None, 0, None, false).unwrap(),
            "0x4e4f2dbd4107c69783e251b7e1057d9f2b9d11cee213441ccfa2be63516dc5bc",
            "0x706c656b23428c8ba356d68db207e11139ede1670481a9e01ae2dfcdb0e1a678",
            27,
        );
    }

    #[test]
    fn test_float_to_int_for_hashing() {
        // The SDK test passes the Python *int* 123123123123 (exact integer
        // arithmetic -> 12312312312300000000). With a float input the SDK
        // returns 12312312312299999232 (checked with the SDK 0.24.0), which is
        // what an f64 port must give.
        assert_eq!(
            float_to_int_for_hashing(123123123123.0).unwrap(),
            12312312312299999232
        );
        assert_eq!(float_to_int_for_hashing(0.00001231).unwrap(), 1231);
        assert_eq!(float_to_int_for_hashing(1.033).unwrap(), 103300000);
        assert!(float_to_int_for_hashing(0.000012312312).is_err());
        assert_eq!(float_to_usd_int(1.5).unwrap(), 1_500_000);
    }

    #[test]
    fn float_to_wire_basics() {
        for (x, w) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (100.0, "100"),
            (104.57, "104.57"),
            (0.0147, "0.0147"),
            (1670.1, "1670.1"),
            (1e-8, "0.00000001"),
            (0.1 + 0.2, "0.3"),
            (1e15, "1000000000000000"),
        ] {
            assert_eq!(float_to_wire(x).unwrap(), w, "{x}");
        }
        assert!(float_to_wire(3.0000000001).is_err());
        assert!(float_to_wire(2500.0 / 104.57).is_err());
        assert!(float_to_wire(f64::NAN).is_err());
        assert!(float_to_wire(f64::INFINITY).is_err());
    }

    #[test]
    fn signature_roundtrip_and_recover() {
        let w = wallet();
        let a = dummy();
        let sig = sign_l1_action(&w, &a, None, 42, Some(99), false).unwrap();
        assert_eq!(
            recover_l1_signer(&a, &sig, None, 42, Some(99), false).unwrap(),
            w.address()
        );
        // A different network or nonce recovers someone else.
        assert_ne!(
            recover_l1_signer(&a, &sig, None, 42, Some(99), true).unwrap(),
            w.address()
        );
        assert_ne!(
            recover_l1_signer(&a, &sig, None, 43, Some(99), false).unwrap(),
            w.address()
        );
        let j = serde_json::to_value(sig).unwrap();
        assert_eq!(j["v"], sig.v);
        let back: Signature = serde_json::from_value(j).unwrap();
        assert_eq!(back, sig);
    }

    #[test]
    fn wallet_debug_redacts_and_rejects_bad_keys() {
        let s = format!("{:?}", wallet());
        assert!(!s.contains("0123456789012345678901234567890123456789"));
        assert!(Wallet::from_hex("0x1234").is_err());
        assert!(Wallet::from_hex(&format!("0x{}", "0".repeat(64))).is_err()); // zero scalar
        assert!(Wallet::from_hex(&format!("0x{}", "zz".repeat(32))).is_err());
        // bare hex accepted
        assert_eq!(
            Wallet::from_hex(&SDK_TEST_KEY[2..]).unwrap().address(),
            wallet().address()
        );
    }

    #[test]
    fn domain_separator_is_stable() {
        // keccak of the encoded Exchange domain (chainId 1337, zero contract).
        let d = exchange_domain_separator();
        assert_eq!(d.len(), 32);
        assert_ne!(d, [0u8; 32]);
        assert_eq!(phantom_source(true), "a");
        assert_eq!(phantom_source(false), "b");
    }
}
