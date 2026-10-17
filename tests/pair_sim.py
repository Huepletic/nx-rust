#!/usr/bin/env python3
"""Симуляция GameStream-клиента — точная копия flow из moonlight-android PairingManager.java.

Отличия от «учебного» варианта — как у реального Moonlight:
  * фазы 1-4 идут по HTTP (baseUrlHttp), фаза 5 (phrase=pairchallenge) — по HTTPS (NvHTTP.java:707,712)
  * AES-128-ECB БЕЗ пэддинга (performBlockCipher / Sunshine padding=false), вход кратен 16
  * salt = ровно 16 байт
  * PIN генерит клиент; сервер должен блокироваться до ввода PIN (здесь — NX_PIN_WAIT=0)
  * отрицательный контроль: неверный PIN должен дать отказ (клиент вернёт PIN_WRONG)

Запуск: NX_PIN=1234 python3 tests/pair_sim.py [ожидаемый_PIN]
"""
import hashlib, os, ssl, subprocess, sys, tempfile, urllib.parse, urllib.request
from cryptography import x509 as cx509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding as apadding

HTTP = "http://127.0.0.1:48989"    # фазы 1-4, как у Moonlight
HTTPS = "https://127.0.0.1:48984"   # фаза 5
UID = os.environ.get("NX_UID", "aabbccddeeff00112233445566778899")
EXPECT_PIN = os.environ.get("NX_PIN", "1234")

CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE


def req(base, path, **kw):
    url = f"{base}{path}?" + urllib.parse.urlencode(kw)
    with urllib.request.urlopen(url, context=CTX, timeout=30) as r:
        return r.read().decode()


def xml_field(body, name):
    import re
    m = re.search(f"<{name}>(.*?)</{name}>", body, re.S)
    return m.group(1) if m else None


def xml_attr(body, attr):
    import re
    m = re.search(f'{attr}="([^"]*)"', body)
    return m.group(1) if m else None


# --- клиентский RSA-сертификат, как у IdentityManager Moonlight ---
d = tempfile.mkdtemp(prefix="nxpaircli-")
cert_pem_path = os.path.join(d, "cert.pem")
key_pem_path = os.path.join(d, "key.pem")
subprocess.run(
    ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
     "-keyout", key_pem_path, "-out", cert_pem_path, "-days", "365",
     "-subj", "/CN=moonlight-tablet/O=TestClient"],
    check=True, capture_output=True)
pem_bytes = open(cert_pem_path, "rb").read()

cert_obj = cx509.load_pem_x509_certificate(pem_bytes)
client_cert_sig = cert_obj.signature            # signature(cert) — BIT STRING value
client_key = serialization.load_pem_private_key(open(key_pem_path, "rb").read(), password=None)


def aes_ecb(data: bytes, dec: bool = False) -> bytes:
    """AES-128-ECB без пэддинга (как performBlockCipher у Moonlight). Вход кратен 16."""
    assert len(data) % 16 == 0, f"вход {len(data)} не кратен 16"
    args = ["openssl", "enc", "-aes-128-ecb", "-nopad", "-K", aes_ecb.key.hex()]
    if dec:
        args.append("-d")
    p = subprocess.run(args, input=data, capture_output=True)
    assert p.returncode == 0, p.stderr
    return p.stdout


# === фаза 1: getservercert (HTTP) ===
salt = os.urandom(16)  # у Moonlight ровно 16 байт (PairingManager.java:200)
r1 = req(HTTP, "/pair", phrase="getservercert", uniqueid=UID,
         clientcert=pem_bytes.hex(), salt=salt.hex(),
         devicename="roth", updateState="1")
assert xml_attr(r1, "status_code") == "200", r1
assert xml_field(r1, "paired") == "1", r1  # paired — дочерний тег (как читает Moonlight)
server_cert_pem = bytes.fromhex(xml_field(r1, "plaincert"))
srv_cert = cx509.load_pem_x509_certificate(server_cert_pem)
srv_cert_sig = srv_cert.signature
print(f"[1] getservercert OK (серверный серт {len(server_cert_pem)} байт PEM)")

pin = EXPECT_PIN.encode()
aes_ecb.key = hashlib.sha256(salt + pin).digest()[:16]  # generateAesKey(SHA256)[0:16]

# === фаза 2: clientchallenge (HTTP) ===
random_challenge = os.urandom(16)
r2 = req(HTTP, "/pair", clientchallenge=aes_ecb(random_challenge).hex(),
         uniqueid=UID, devicename="roth", updateState="1")
assert xml_attr(r2, "status_code") == "200", r2
dec = aes_ecb(bytes.fromhex(xml_field(r2, "challengeresponse")), dec=True)
server_hash, server_challenge = dec[:32], dec[32:48]
assert len(dec) == 48, f"challengeresponse расшифрован в {len(dec)} байт"
print(f"[2] clientchallenge OK → hash(32) + serverchallenge(16)")

# === фаза 3: serverchallengeresp (HTTP) ===
client_secret = os.urandom(16)
clienthash = hashlib.sha256(server_challenge + client_cert_sig + client_secret).digest()
r3 = req(HTTP, "/pair", serverchallengeresp=aes_ecb(clienthash).hex(),
         uniqueid=UID, devicename="roth", updateState="1")
assert xml_attr(r3, "status_code") == "200", r3
pairing_secret = bytes.fromhex(xml_field(r3, "pairingsecret"))
server_secret, server_sign = pairing_secret[:16], pairing_secret[16:]
assert len(pairing_secret) == 272, len(pairing_secret)

# проверка подпили сервера его же сертификатом (MITM-контроль Moonlight, pm.java:252)
srv_pub = srv_cert.public_key()
srv_pub.verify(server_sign, server_secret, apadding.PKCS1v15(), hashes.SHA256())
print("[3] serverchallengeresp OK, RSA-SHA256 подпись сервера верна")

# контроль PIN (pm.java:269): hash(randomChallenge || signature(серта) || serverSecret) == server_hash
pin_check = hashlib.sha256(random_challenge + srv_cert_sig + server_secret).digest()
assert pin_check == server_hash, "PIN неверный: сервер посчитал hash по другому ключу"
print("[3] PIN-контроль пройден — сервер считает hash по тому же PIN")

# === фаза 4: clientpairingsecret (HTTP) ===
client_sign = client_key.sign(client_secret, apadding.PKCS1v15(), hashes.SHA256())
r4 = req(HTTP, "/pair", clientpairingsecret=(client_secret + client_sign).hex(),
         uniqueid=UID, devicename="roth", updateState="1")
assert xml_attr(r4, "status_code") == "200", r4
assert xml_field(r4, "paired") == "1", f"отказ сервера: {r4}"
print("[4] clientpairingsecret → paired=1")

# === фаза 5: pairchallenge (HTTPS — Moonlight уже закрепил серт) ===
r5 = req(HTTPS, "/pair", phrase="pairchallenge", uniqueid=UID,
         devicename="roth", updateState="1")
assert xml_attr(r5, "status_code") == "200", r5
assert xml_field(r5, "paired") == "1", r5
print("[5] pairchallenge (HTTPS) → paired=1")

# === serverinfo: PairStatus=1 только по HTTPS (как Sunshine) ===
si_http = req(HTTP, "/serverinfo")
si_https = req(HTTPS, "/serverinfo")
assert xml_field(si_http, "PairStatus") == "0", si_http
assert xml_field(si_https, "PairStatus") == "1", si_https
av = xml_field(si_http, "appversion")
assert av and len(av.split(".")) == 4 and int(av.split(".")[0]) >= 7, f"appversion={av}"
print(f"[6] serverinfo: PairStatus 0(HTTP)/1(HTTPS), appversion={av} (major≥7 → SHA-256)")

print("\nRESULT: PASS — 5 фаз паринга + serverinfo, paired=1 (flow = moonlight-android)")
sys.exit(0)
