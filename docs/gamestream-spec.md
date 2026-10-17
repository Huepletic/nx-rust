# GameStream-протокол: рабочая спека (из кода Sunshine master, LizardByte)

Источники (скачаны в docs/research/):
- src/nvhttp.cpp, src/nvhttp.h — NVHTTP, паринг
- src/rtsp.cpp — RTSP-хендшейк
- src/stream.cpp, src/stream.h — control-канал (ENet), RTP-видео, шифрование
- src/input.cpp — input-пакеты
- src/main.cpp — порты

## 1. Порты (базовый = 47989, config::sunshine.port)

| Слой | Порт | Транспорт |
|---|---|---|
| HTTP (redirect, discovery) | 47989 (PORT_HTTP = base+0) | TCP |
| HTTPS (NVHTTP: pair/serverinfo/launch) | 47984 (PORT_HTTPS = base−5) | TLS |
| RTSP | 47990 (RTSP_SETUP_PORT) | TCP |
| Video RTP | 47998 (base+9, VIDEO_STREAM_PORT) | UDP |
| Control (ENet) | 47999 (base+10, CONTROL_PORT) | UDP |
| Audio RTP (Opus) | 48000 (base+11, AUDIO_STREAM_PORT) | UDP |
| mDNS | 5353 | UDP (_nvstream._tcp) |

## 2. NVHTTP endpoints (XML, status_code в атрибутах корня)

- **/serverinfo** (HTTP и HTTPS): hostname, appversion, GfeVersion, uniqueid, HttpsPort, ExternalPort,
  MaxLumaPixelsHEVC, mac (HTTP → 00:00:00:00:00:00), LocalIP, ServerCodecModeSupport (битовая маска
  SCM_H264=1, SCM_H264_HIGH8_444=2, SCM_HEVC=4, ...), PairStatus (1 если uniqueid передан),
  currentgame (>0 = занято), state ("SUNSHINE_SERVER_FREE"/"..._BUSY").
- **/applist** (только HTTPS): root/App[] c AppTitle, ID, IsHdrSupported.
- **/pair** (HTTPS, все фазы — см. §3), query: uniqueid, phrase, salt, clientcert, devicename...
- **/launch** (HTTPS): rikey (hex 16B — AES-GCM ключ видеопотока), rikeyid (u32 BE — префикс IV),
  mode=WxHxFPS, appid, localAudioPlayMode, sops, surroundAudioInfo, corever (>=1 → шифрованный RTSP,
  схема rtspenc://), uniqueid. Ответ: resume=0, XML c gameSession/hostname/httpsPort/rtspUrl.
- **/resume**, **/cancel**, **/clear**, **/poll** (текущая игра для poll'а).

## 3. Паринг (4 фазы, AES-128-ECB-контейнер + подписи)

Клиент шлёт /pair?uniqueid=U&phrase=getservercert&clientcert=<HEX PEM>&salt=<hex32>&devicename=...
1. **getservercert**: сервер генерит AES-ключ из salt[0..16] + pin (PBKDF2-ish `gen_aes_key(salt, pin)`),
   возвращает plaincert = HEX(сертификат сервера).
2. **clientchallenge**: клиент шлёт challenge, зашифрованный AES-ECB этим ключом. Сервер расшифровывает,
   приписывает signature(серверный серт) + serversecret(16B rand), хэширует (SHA-256), добавляет
   serverchallenge(16B rand), шифрует → challengeresponse.
3. **serverchallengeresp**: клиент присылает свой hash (проверка против MITM) → сервер отвечает
   pairingsecret = serversecret + sign256(серверный приватный ключ, serversecret).
4. **clientpairingsecret**: клиент шлёт secret(16B) + sign256(клиентский ключ, secret). Сервер проверяет:
   hash(serverchallenge + signature(клиентского серта) + secret) == clienthash из фазы 2,
   verify256(клиентский серт, secret, sign). ОК → клиент добавлен в authorized certs (JSON clients.json).
- pin вводится на сервере (веб-UI/tray/stdin).
- Последующие HTTPS-запросы: клиент шлёт свой приватный ключ+сертификат в TLS-хендшейке;
  сервер верифицирует через cert_chain (client-как-MTLS) + сверяет с включённым client'ом.

## 4. RTSP (порт 47990, обычный текстовый RTSP)

- **OPTIONS** → OK.
- **DESCRIBE** → SDP-подобный body: `a=x-ss-general.featureFlags`, `x-ss-general.encryptionSupported/Requested`,
  `x-nv-video[0].refPicInvalidation`, `sprop-parameter-sets` (если HEVC), `a=rtpmap:98 AV1/90000` (если AV1),
  `a=fmtp:97 surround-params=...` (аудио, 3 раза для 51/71).
- **SETUP** (3 вызова: streamid=audio/... , video/..., control/...) → `Session: DEADBEEFCAFE;timeout=90`,
  `Transport: server_port=<port>`, `X-SS-Connect-Data` (control) или `X-SS-Ping-Payload` (audio/video).
- **ANNOUNCE** — клиент шлёт SDP с ключами `a=x-nv-video[0].packetSize`, `x-nv-audio.surround.numChannels`,
  `x-nv-general.useReliableUdp`, `x-nv-vqos[0].bitStreamFormat` (0=H264,1=HEVC), `x-ss-general.encryptionEnabled`,
  `x-ml-general.featureFlags`, `x-nv-video[0].encoderCscMode`, `x-nv-video[0].dynamicRangeMode`,
  `x-nv-aqos.packetDuration` (default 5), `x-nv-vqos[0].fec.minRequiredFecPackets` (default 0).
  Дефолты при отсутствии — см. try_emplace в cmd_announce (rtsp.cpp:1090-1110).
- **PLAY** → старт стриминга. После PLAY клиент дёргает launch-URL (rtsp://host:47990/launch?id=...&clientId=...).

## 5. Control-канал (UDP 47999, ENet reliable!)

Реализован через ENet (reliable UDP библиотека). В Rust: крейт `renet`/`renet_net` или libenet FFI.
Типы пакетов (u16 LE, packetTypes в stream.cpp:58):
| Тип | Назначение |
|---|---|
| 0x0305/0x0307 | Start A/B (handshake после PLAY) |
| 0x0301 | Invalidate reference frames (запрос IDR-области) |
| 0x0201 | Loss stats |
| 0x0206 | Input data (входы от клиента) |
| 0x010b | Rumble |
| 0x0109 | Termination |
| 0x0200 | Periodic PING (клиент пингует — сервер обязан отвечать) |
| 0x0302 | IDR frame request |
| 0x0001 | Fully encrypted |
| 0x010e | HDR mode |

Шифрование control/аудио/видео: AES-128-GCM, ключ = gcm_key из rikey (launch),
IV = 12 байт: первые 8 байт из rikeyid (u32 BE префикс) + счётчик (iv_counter) / seq.
`SS_ENC_CONTROL_V2` (0x2), `SS_ENC_AUDIO` (0x1), `SS_ENC_VIDEO` (0x4) — биты encryptionSupported/Requested в DESCRIBE.
`x-nv-general.featureFlags & 0x20` → клиент поддерживает аудио-шифрование (legacy).

## 6. Видеопоток (UDP 47998)

- RTP-подобный формат Sunshine: заголовок с sequence/timestamp/ptype, payload = NAL-юниты H264/HEVC.
- FEC: Reed-Solomon шарды (RTPA_DATA_SHARDS), интерливинг. minRequiredFecPackets из ANNOUNCE.
- ptype видео: 0x0302 = IDR-фрейм. Moonlight требует: первый фрейм после старта — IDR, и IDR по запросу 0x0302.
- Шифрование: SS_ENC_VIDEO → префикс 12-байтовый IV перед payload + AES-GCM (см. stream.cpp:157-163).

## 7. Аудио (UDP 48000)

Opus 48 кГц, surround-params из DESCRIBE. Можно отключить host_audio; аудио-шифрование опционально.
Полное отключение аудио-потока Moonlight терпит (surround.numChannels=0), но лучше отдать стаб Opus.

## 8. mDNS

Service type: `_nvstream._tcp`, TXT: поколение GFE; Moonlight на Android ищет именно его.
Можно опустить — Moonlight умеет ручной ввод IP.

## 9. Минимальный путь до картинки (MVP)

1. HTTPS /serverinfo (с cert'ом сервера) — Moonlight видит «ПК».
2. Паринг 4 фазы — Moonlight принимает PIN.
3. /applist (хотя бы одно «Desktop» приложение).
4. /launch → ответ содержит rtspUrl.
5. RTSP: OPTIONS→DESCRIBE→SETUP×3→ANNOUNCE→PLAY.
6. Control ENet: ответить на Start A/B, крутить PING, принимать 0x0206 input.
7. Video: RTP на клиентский порт, первый фрейм IDR, AES-GCM по желанию (можно начать encryptionEnabled=0).
8. Audio: минимальный Opus-поток либо отключить.
Без аудио и без FEC Moonlight обычно работает; с encryptionEnabled=0 — тоже.

## 10. Что используем из Sunshine как «чёрный ящик» (для проверки)

- Пути исходников в docs/research/*.cpp (с указанием строк выше) — источник истины при расхождении.
- moonlight-common-c: src/Connection.c, Limelight.h — RTSP-клиент и разбор видео (будем сверяться).
