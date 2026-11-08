//! Apple's SRP-6a variant. Protocol reference: https://github.com/rhoopr/kei
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use num_bigint::BigUint;
use rand::RngExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const PRIME: &str = concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC319294",
    "3DB56050A37329CBB4A099ED8193E0757767A13DD52312AB4B03310D",
    "CD7F48A9DA04FD50E8083969EDB767B0CF6095179A163AB3661A05FB",
    "D5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF74",
    "7359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A",
    "436C6481F1D2B9078717461A5B9D32E688F87748544523B524B0D57D",
    "5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E73",
    "03CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB6",
    "94B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73"
);

#[derive(Deserialize)]
pub struct Challenge {
    pub salt: String,
    pub b: String,
    pub c: String,
    pub iteration: u32,
    pub protocol: String,
}

pub struct Srp {
    n: BigUint,
    private: BigUint,
    public: BigUint,
}

fn hash(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = Sha256::new();
    for part in parts {
        h.update(part);
    }
    h.finalize().to_vec()
}

fn pad(n: &BigUint) -> Result<Vec<u8>> {
    let bytes = n.to_bytes_be();
    ensure!(bytes.len() <= 256, "无效的 SRP 公钥长度");
    let mut result = vec![0; 256 - bytes.len()];
    result.extend(bytes);
    Ok(result)
}

impl Srp {
    pub fn new() -> Self {
        let n = BigUint::parse_bytes(PRIME.as_bytes(), 16).expect("constant SRP prime");
        let mut secret = [0u8; 32];
        rand::rng().fill(&mut secret);
        let private = BigUint::from_bytes_be(&secret);
        let public = BigUint::from(2u32).modpow(&private, &n);
        Self { n, private, public }
    }

    pub fn public_key(&self) -> String {
        B64.encode(self.public.to_bytes_be())
    }

    pub fn proofs(
        &self,
        username: &str,
        password: &str,
        ch: &Challenge,
    ) -> Result<(String, String)> {
        ensure!(
            (1..=1_000_000).contains(&ch.iteration),
            "SRP 迭代次数超出限制"
        );
        ensure!(
            matches!(ch.protocol.as_str(), "s2k" | "s2k_fo"),
            "不支持的 SRP 协议"
        );
        let salt = B64.decode(&ch.salt).context("SRP salt 无效")?;
        let b = BigUint::from_bytes_be(&B64.decode(&ch.b).context("SRP B 无效")?);
        ensure!(b < self.n && b != BigUint::ZERO, "无效的 SRP 服务端公钥");
        let digest = hash(&[password.as_bytes()]);
        let input = if ch.protocol == "s2k_fo" {
            digest
                .iter()
                .map(|v| format!("{v:02x}"))
                .collect::<String>()
                .into_bytes()
        } else {
            digest
        };
        let mut derived = [0; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(&input, &salt, ch.iteration, &mut derived);
        let x = BigUint::from_bytes_be(&hash(&[&salt, &hash(&[b":", &derived])]));
        let g = BigUint::from(2u32);
        let k = BigUint::from_bytes_be(&hash(&[&self.n.to_bytes_be(), &pad(&g)?]));
        let u = BigUint::from_bytes_be(&hash(&[&pad(&self.public)?, &pad(&b)?]));
        ensure!(u != BigUint::ZERO, "无效的 SRP challenge");
        let base = (&b + &self.n - (k * g.modpow(&x, &self.n)) % &self.n) % &self.n;
        let shared = base.modpow(&(&self.private + u * x), &self.n);
        let key = hash(&[&shared.to_bytes_be()]);
        let hn = hash(&[&self.n.to_bytes_be()]);
        let hg = hash(&[&pad(&g)?]);
        let xor: Vec<_> = hn.iter().zip(hg).map(|(a, b)| a ^ b).collect();
        let a_bytes = self.public.to_bytes_be();
        let m1 = hash(&[
            &xor,
            &hash(&[username.as_bytes()]),
            &salt,
            &a_bytes,
            &b.to_bytes_be(),
            &key,
        ]);
        let m2 = hash(&[&a_bytes, &m1, &key]);
        Ok((B64.encode(m1), B64.encode(m2)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent server side: verify the client's shared secret and M1.
    #[test]
    fn agrees_with_server_for_both_password_protocols() {
        for protocol in ["s2k", "s2k_fo"] {
            let client = Srp::new();
            let salt = b"test-salt";
            let password_hash = Sha256::digest(b"password");
            let input = if protocol == "s2k_fo" {
                password_hash
                    .iter()
                    .map(|v| format!("{v:02x}"))
                    .collect::<String>()
                    .into_bytes()
            } else {
                password_hash.to_vec()
            };
            let mut key = [0; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(&input, salt, 10, &mut key);
            let x = BigUint::from_bytes_be(&hash(&[salt, &hash(&[b":", &key])]));
            let g = BigUint::from(2u32);
            let v = g.modpow(&x, &client.n);
            let k = BigUint::from_bytes_be(&hash(&[&client.n.to_bytes_be(), &pad(&g).unwrap()]));
            let secret = BigUint::from(12345u32);
            let b = (k * &v + g.modpow(&secret, &client.n)) % &client.n;
            let u =
                BigUint::from_bytes_be(&hash(&[&pad(&client.public).unwrap(), &pad(&b).unwrap()]));
            let shared = (&client.public * v.modpow(&u, &client.n)).modpow(&secret, &client.n);
            let session_key = hash(&[&shared.to_bytes_be()]);
            let ch = Challenge {
                salt: B64.encode(salt),
                b: B64.encode(b.to_bytes_be()),
                c: String::new(),
                iteration: 10,
                protocol: protocol.into(),
            };
            let (m1, m2) = client.proofs("test@icloud.com", "password", &ch).unwrap();
            let xor: Vec<_> = hash(&[&client.n.to_bytes_be()])
                .iter()
                .zip(hash(&[&pad(&g).unwrap()]))
                .map(|(a, b)| a ^ b)
                .collect();
            assert_eq!(
                B64.decode(&m1).unwrap(),
                hash(&[
                    &xor,
                    &hash(&[b"test@icloud.com"]),
                    salt,
                    &client.public.to_bytes_be(),
                    &b.to_bytes_be(),
                    &session_key
                ])
            );
            assert_eq!(
                B64.decode(m2).unwrap(),
                hash(&[
                    &client.public.to_bytes_be(),
                    &B64.decode(m1).unwrap(),
                    &session_key
                ])
            );
            let bad = Challenge {
                b: B64.encode([0]),
                ..ch
            };
            assert!(client.proofs("test@icloud.com", "password", &bad).is_err());
        }
    }
}
