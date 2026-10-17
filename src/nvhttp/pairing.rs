//! pairing: 4-фазный GameStream-паринг (по Sunshine nvhttp.cpp).
//! 1. getservercert   → ключ = SHA256(salt||pin)[0..16], отдаём plaincert
//! 2. clientchallenge → расшифровка AES-128-ECB, ответ hash||serverchallenge (в ECB)
//! 3. serverchallengeresp → pairingsecret = serversecret || RSA-SHA256(серверный ключ, serversecret)
//! 4. clientpairingsecret → проверка hash(serverchallenge||signature(клиентского серта)||secret)
//!                           и verify256(клиентский серт, secret, sign)

use aes::Aes128;
use anyhow::{anyhow, Result};
use cipher::block::{BlockModeDecrypt, BlockModeEncrypt};
use cipher::KeyInit;
use ecb::Decryptor as EcbDec;
use ecb::Encryptor as EcbEnc;
use hybrid_array::Array;
use sha2::{Digest, Sha256};

use crate::nvhttp::{PairedClient, ServerState};

type AesEcbEnc = EcbEnc<Aes128>;
type AesEcbDec = EcbDec<Aes128>;

pub enum Phase {
    Start,
    GetServerCert,
    ClientChallenge,
    ServerChallengeResp,
}

pub struct PairingState {
    pub phase: Phase,
    pub cipher_key: [u8; 16],
    pub serversecret: Vec<u8>,
    pub serverchallenge: Vec<u8>,
    pub clienthash: Vec<u8>,
    pub client_cert_der: Vec<u8>,
    pub client_name: String,
}

/// PIN для MVP — передаётся в ServerState.pin; mutex-состояние нужно для многопоточности.
pub fn handle(
    st: &ServerState,
    phrase: &str,
    p: &crate::nvhttp::PairParams,
) -> Result<String, (u32, String)> {
    // Диспетчеризация по аргументам, как в Sunshine (nvhttp.cpp:1121):
    // клиент шлёт phrase=pairchallenge для фаз 2-4, различие — по наличию поля.
    if !p.clientchallenge.is_empty() {
        clientchallenge(st, p)
    } else if !p.serverchallengeresp.is_empty() {
        serverchallengeresp(st, p)
    } else if !p.clientpairingsecret.is_empty() {
        clientpairingsecret(st, p)
    } else {
        match phrase {
            "getservercert" => getservercert(st, p),
            // Фаза 5 (Sunshine nvhttp.cpp:1103): финальный вызов клиента после clientpairingsecret
            "pairchallenge" => Ok(pairchallenge_xml()),
            _ => Err((400, "Invalid pairing request".into())),
        }
    }
}

fn getservercert(st: &ServerState, p: &crate::nvhttp::PairParams) -> Result<String, (u32, String)> {
    let salt = hex::decode(&p.salt).map_err(|_| (400, "bad salt".into()))?;
    if salt.len() < 16 {
        return Err((400, "Salt too short".into()));
    }
    let salt16: [u8; 16] = salt[0..16].try_into().unwrap();
    // ключ = SHA256(salt16 || pin)[0..16]
    let pin = st.pin.lock().unwrap().clone();
    let mut h = Sha256::new();
    h.update(salt16);
    h.update(pin.as_bytes());
    let digest = h.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[0..16]);

    let mut map = st.pairing.lock().unwrap();
    map.insert(
        p.uniqueid.clone(),
        PairingState {
            phase: Phase::GetServerCert,
            cipher_key: key,
            serversecret: vec![],
            serverchallenge: vec![],
            clienthash: vec![],
            client_cert_der: hex::decode(&p.clientcert).unwrap_or_default(),
            client_name: p.devicename.clone(),
        },
    );
    Ok(xml_pair(1, &format!(
        "<plaincert>{}</plaincert>",
        hex::encode(st.server_cert_pem.as_bytes())
    )))
}

fn clientchallenge(st: &ServerState, p: &crate::nvhttp::PairParams) -> Result<String, (u32, String)> {
    let mut map = st.pairing.lock().unwrap();
    let ps = map
        .get_mut(&p.uniqueid)
        .ok_or((400, "Invalid uniqueid".into()))?;
    if !matches!(ps.phase, Phase::GetServerCert) {
        return Err((400, "Out of order call to clientchallenge".into()));
    }
    let challenge = hex::decode(&p.clientchallenge).map_err(|_| (400, "bad challenge".into()))?;
    let decrypted = aes_ecb_decrypt(&ps.cipher_key, &challenge);

    // hash = SHA256(decrypted || signature(серверного серта) || serversecret)
    let serversecret = rand16();
    let sign = cert_signature_der(&st.server_cert_pem).map_err(|e| (500, e.to_string()))?;
    let mut data = decrypted.clone();
    data.extend_from_slice(&sign);
    data.extend_from_slice(&serversecret);
    let hash = Sha256::digest(&data).to_vec();

    let serverchallenge = rand16();
    let mut plain = hash;
    plain.extend_from_slice(&serverchallenge);
    let encrypted = aes_ecb_encrypt(&ps.cipher_key, &plain);

    ps.serversecret = serversecret;
    ps.serverchallenge = serverchallenge;
    ps.phase = Phase::ClientChallenge;

    Ok(xml_pair(1, &format!(
        "<challengeresponse>{}</challengeresponse>",
        hex::encode(&encrypted)
    )))
}

fn serverchallengeresp(
    st: &ServerState,
    p: &crate::nvhttp::PairParams,
) -> Result<String, (u32, String)> {
    let mut map = st.pairing.lock().unwrap();
    let ps = map
        .get_mut(&p.uniqueid)
        .ok_or((400, "Invalid uniqueid".into()))?;
    if !matches!(ps.phase, Phase::ClientChallenge) {
        return Err((400, "Out of order call to serverchallengeresp".into()));
    }
    let enc = hex::decode(&p.serverchallengeresp).map_err(|_| (400, "bad resp".into()))?;
    let decrypted = aes_ecb_decrypt(&ps.cipher_key, &enc);
    ps.clienthash = decrypted;
    ps.phase = Phase::ServerChallengeResp;

    // pairingsecret = serversecret || RSA-SHA256(приватный ключ сервера, serversecret)
    let secret = ps.serversecret.clone();
    let sign = rsa_sign_sha256(&st.server_key_pem, &secret).map_err(|e| (500, e.to_string()))?;
    let mut out = secret;
    out.extend_from_slice(&sign);

    Ok(xml_pair(1, &format!(
        "<pairingsecret>{}</pairingsecret>",
        hex::encode(&out)
    )))
}

fn clientpairingsecret(
    st: &ServerState,
    p: &crate::nvhttp::PairParams,
) -> Result<String, (u32, String)> {
    let mut map = st.pairing.lock().unwrap();
    let ps = map
        .get_mut(&p.uniqueid)
        .ok_or((400, "Invalid uniqueid".into()))?;
    if !matches!(ps.phase, Phase::ServerChallengeResp) {
        return Err((400, "Out of order call to clientpairingsecret".into()));
    }
    let blob = hex::decode(&p.clientpairingsecret).map_err(|_| (400, "bad secret".into()))?;
    if blob.len() <= 16 {
        return Err((400, "Client pairing secret too short".into()));
    }
    let secret = &blob[0..16];
    let sign = &blob[16..];

    // hash(serverchallenge || signature(клиентского серта) || secret) == clienthash
    let client_sig = cert_signature_der_bytes(&ps.client_cert_der).map_err(|e| (400, e.to_string()))?;
    let mut data = ps.serverchallenge.clone();
    data.extend_from_slice(&client_sig);
    data.extend_from_slice(secret);
    let hash = Sha256::digest(&data).to_vec();
    let same_hash = hash == ps.clienthash;
    if !same_hash {
        println!(
            "pairing debug: server_hash={} clienthash={} sc_len={} sig_len={} secret_len={}",
            hex::encode(&hash),
            hex::encode(&ps.clienthash),
            ps.serverchallenge.len(),
            client_sig.len(),
            secret.len()
        );
    }

    // verify256(клиентский серт, secret, sign)
    let verify_ok = x509_verify_sha256(&ps.client_cert_der, secret, sign);

    let paired = same_hash && verify_ok;
    if paired {
        // сохраняем клиента
        let cert_pem = String::from_utf8_lossy(&ps.client_cert_der).to_string();
        let uuid = format!("uuid-{}", &p.uniqueid[..8.min(p.uniqueid.len())]);
        let mut clients = st.clients.lock().unwrap();
        clients.push(PairedClient {
            name: ps.client_name.clone(),
            uuid,
            uniqueid: p.uniqueid.clone(),
            cert_pem,
        });
        println!("pairing: клиент '{}' спарен", ps.client_name);
        // Гигиена: PIN использован — убираем канал, чтобы не переиспользовался
        std::fs::remove_file("/tmp/nx-pin").ok();
    } else {
        println!("pairing: отказ (same_hash={} verify={})", same_hash, verify_ok);
    }

    Ok(xml_pair(if paired { 1 } else { 0 }, ""))
}

fn pairchallenge_xml() -> String {
    xml_pair(1, "")
}

fn xml_pair(paired: u32, inner: &str) -> String {
    // paired — ДОЧЕРНИЙ ТЕГ (Sunshine: tree.put("root.paired", ...)), а не атрибут!
    // Moonlight читает его через getXmlString(..., "paired", true) — атрибут он не видит
    // и падает с «Missing mandatory field in host response: paired».
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>{}</paired>{}</root>"#,
        paired, inner
    )
}

// === крипто-помощники ===

fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut cipher = AesEcbEnc::new(key.into());
    // Без пэддинга, как у Sunshine (ecb_t(key, padding=false), crypto.cpp:313) и Moonlight
    // (performBlockCipher — округление нулями до границы блока). Все протокольные
    // payload'ы кратны 16 байтам: challenge=16, hash=32, hash||serverchallenge=48.
    let mut out = data.to_vec();
    let rem = out.len() % 16;
    if rem != 0 {
        out.resize(out.len() + (16 - rem), 0);
    }
    let mut res = Vec::with_capacity(out.len());
    for chunk in out.chunks(16) {
        let mut arr = [0u8; 16];
        arr.copy_from_slice(chunk);
        let mut blk = Array::from(arr);
        cipher.encrypt_block(&mut blk);
        res.extend_from_slice(&blk[..]);
    }
    res
}

fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut cipher = AesEcbDec::new(key.into());
    let mut res = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut arr = [0u8; 16];
        arr.copy_from_slice(chunk);
        let mut b = Array::from(arr);
        cipher.decrypt_block(&mut b);
        res.extend_from_slice(&b[..]);
    }
    // Без PKCS7-среза: клиент шлёт непэддленные блоки (Sunshine padding=false),
    // слепой срез по последнему байту ломал бы ~6% парингов (случайный байт ≤16).
    res
}

fn rand16() -> Vec<u8> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    b.to_vec()
}

/// Подпись BIT STRING из X.509 сертификата (PEM) — «signature(x509)» из Sunshine.
fn cert_signature_der(pem: &str) -> Result<Vec<u8>> {
    cert_signature_der_bytes(pem.as_bytes())
}

/// Сигнатура X.509 (BIT STRING): принимает PEM или DER.
fn cert_signature_der_bytes(cert_pem_or_der: &[u8]) -> Result<Vec<u8>> {
    // PEM?
    if let Ok(s) = std::str::from_utf8(cert_pem_or_der) {
        if s.contains("-----BEGIN CERTIFICATE-----") {
            let (_, pem) = x509_parser::pem::parse_x509_pem(s.as_bytes())
                .map_err(|e| anyhow!("parse pem: {:?}", e))?;
            let cert = pem
                .parse_x509()
                .map_err(|e| anyhow!("pem->x509: {:?}", e))?;
            return Ok(cert.signature_value.data.to_vec());
        }
    }
    let (_, cert) = x509_parser::parse_x509_certificate(cert_pem_or_der)
        .map_err(|e| anyhow!("parse client cert: {}", e))?;
    Ok(cert.signature_value.data.to_vec())
}

/// RSA-SHA256 подпись (PKCS#1 v1.5) приватным ключом сервера (PEM).
fn rsa_sign_sha256(key_pem: &str, data: &[u8]) -> Result<Vec<u8>> {
    sign_via_files(key_pem, data)
}

fn sign_via_files(key_pem: &str, data: &[u8]) -> Result<Vec<u8>> {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("nxpair-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let key_path = dir.join("key.pem");
    let data_path = dir.join("data.bin");
    let sig_path = dir.join("sig.bin");
    std::fs::write(&key_path, key_pem)?;
    std::fs::write(&data_path, data)?;
    let out = Command::new("openssl")
        .args([
            "dgst",
            "-sha256",
            "-sign",
            key_path.to_str().unwrap(),
            "-out",
            sig_path.to_str().unwrap(),
        ])
        .arg(data_path.to_str().unwrap())
        .output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "openssl sign: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let sig = std::fs::read(&sig_path)?;
    std::fs::remove_dir_all(&dir).ok();
    Ok(sig)
}

/// verify256(клиентский серт PEM/DER, данные, подпись) через openssl.
fn x509_verify_sha256(cert: &[u8], data: &[u8], sig: &[u8]) -> bool {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("nxverify-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let cert_path = dir.join("cert.pem");
    let pub_path = dir.join("pub.pem");
    let data_path = dir.join("data.bin");
    let sig_path = dir.join("sig.bin");
    let _ = std::fs::write(&cert_path, cert);
    let _ = std::fs::write(&data_path, data);
    let _ = std::fs::write(&sig_path, sig);
    let extract = Command::new("openssl")
        .args(["x509", "-pubkey", "-noout", "-in"])
        .arg(cert_path.to_str().unwrap())
        .arg("-out")
        .arg(pub_path.to_str().unwrap())
        .output();
    if !matches!(extract, Ok(o) if o.status.success()) {
        let _ = std::fs::remove_dir_all(&dir);
        return false;
    }
    let verify = Command::new("openssl")
        .args(["dgst", "-sha256", "-verify"])
        .arg(pub_path.to_str().unwrap())
        .arg("-signature")
        .arg(sig_path.to_str().unwrap())
        .arg(data_path.to_str().unwrap())
        .output();
    let ok = matches!(verify, Ok(o) if o.status.success());
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip() {
        let key: [u8; 16] = [7u8; 16];
        for n in [1usize, 15, 16, 17, 32, 100] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            let enc = aes_ecb_encrypt(&key, &data);
            assert_eq!(enc.len() % 16, 0, "шифртекст должен выравниваться по 16");
            // без пэддинга: расшифровка вернёт данные, дополненные нулями до блока
            let dec = aes_ecb_decrypt(&key, &enc);
            let mut expect = data.clone();
            let rem = expect.len() % 16;
            if rem != 0 {
                expect.resize(expect.len() + (16 - rem), 0);
            }
            assert_eq!(dec, expect, "roundtrip для len={}", n);
        }
    }

    #[test]
    fn aes_matches_openssl_ecb() {
        // сверка с эталоном OpenSSL: AES-128-ECB без пэддинга (нэш padding=false),
        // вход кратен 16 — как все протокольные payload'ы.
        use std::process::Command;
        let key: [u8; 16] = *b"0123456789abcdef";
        let data = b"GameStreamPair!!"; // ровно 16 байт
        assert_eq!(data.len(), 16);
        let mine = aes_ecb_encrypt(&key, data);

        let dir = std::env::temp_dir().join("nxesecb");
        std::fs::create_dir_all(&dir).unwrap();
        let inp = dir.join("in.bin");
        let outp = dir.join("out.bin");
        std::fs::write(&inp, data).unwrap();
        let out = Command::new("openssl")
            .args(["enc", "-aes-128-ecb", "-nopad", "-K", &hex::encode(key), "-in"])
            .arg(&inp.to_str().unwrap())
            .arg("-out")
            .arg(&outp.to_str().unwrap())
            .output()
            .unwrap();
        assert!(out.status.success(), "openssl enc failed: {}", String::from_utf8_lossy(&out.stderr));
        let golden = std::fs::read(&outp).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(mine, golden, "шифртекст должен байт-в-байт совпадать с OpenSSL AES-128-ECB");
    }

    #[test]
    fn rsa_sign_verifies_with_openssl() {
        let (cert, key) = crate::nvhttp::gen_selfsigned().unwrap();
        let data = b"serversecret-bytes";
        let sig = rsa_sign_sha256(&key, data).unwrap();
        assert!(!sig.is_empty(), "подпись не пустая");
        assert!(x509_verify_sha256(cert.as_bytes(), data, &sig), "подпись верифицируется сертификатом");
        // негативный контроль
        let mut bad = sig.clone();
        bad[0] ^= 0xff;
        assert!(!x509_verify_sha256(cert.as_bytes(), data, &bad), "битая подпись отвергается");
    }
}
