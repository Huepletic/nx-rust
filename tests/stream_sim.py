#!/usr/bin/env python3
"""stream_sim: проверка M3-конвейера без планшета.

1) HTTPS /launch → sessionUrl0, launch-сессия создана.
2) RTSP: OPTIONS → DESCRIBE → SETUP audio/video/control → ANNOUNCE → PLAY.
   Каждый запрос — своё TCP-соединение (как Moonlight), проверяем заголовки.
3) SS_PING со своего UDP-сокета на video-порт → ждём RTP, валидируем формат:
   длина = packetSize+16, header=0x90, NV_VIDEO_PACKET, SOF/EOF, lowseq,
   собираем кадр 0 из чанков → Annex-B → SPS.
"""
import socket
import ssl
import struct
import sys
import time
import urllib.request

HOST = "127.0.0.1"
HTTPS_PORT = 48984
RTSP_PORT = 49010
VIDEO_PORT = 49000
PACKET_SIZE = 1024
BLOCKSIZE = PACKET_SIZE + 16  # ожидаемая длина UDP-пакета

failures = []


def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        failures.append(msg)


def http_get(path):
    ctx = ssl._create_unverified_context()
    url = f"https://{HOST}:{HTTPS_PORT}{path}"
    with urllib.request.urlopen(url, context=ctx, timeout=10) as r:
        return r.read().decode()


def rtsp(method, target, body=None, extra_headers=()):
    s = socket.create_connection((HOST, RTSP_PORT), timeout=10)
    lines = [f"{method} {target} RTSP/1.0", "CSeq: 1"]
    for h in extra_headers:
        lines.append(h)
    if body is not None:
        lines.append(f"Content-Length: {len(body)}")
    req = ("\r\n".join(lines) + "\r\n\r\n").encode()
    if body is not None:
        req += body
    s.sendall(req)
    chunks = []
    while True:
        d = s.recv(65536)
        if not d:
            break
        chunks.append(d)
    s.close()
    raw = b"".join(chunks)
    head, _, payload = raw.partition(b"\r\n\r\n")
    status = head.split(b"\r\n")[0].decode(errors="replace")
    headers = {}
    for line in head.split(b"\r\n")[1:]:
        k, _, v = line.decode(errors="replace").partition(":")
        headers[k.strip().lower()] = v.strip()
    return status, headers, payload


def main():
    print("== 1) /launch ==")
    body = http_get(
        "/launch?appid=1&mode=1280x720x60&rikey=" + "ab" * 16 + "&rikeyid=deadbeef"
    )
    print("  launch:", body[:300])
    check('status_code="200"' in body, "launch status_code=200")
    check("<gamesession>1</gamesession>" in body, "gamesession (строчная s)")
    check(f"<sessionUrl0>rtsp://{HOST}:{RTSP_PORT}</sessionUrl0>" in body, "sessionUrl0")
    time.sleep(0.3)

    print("== 2) RTSP handshake ==")
    st, hd, _ = rtsp("OPTIONS", f"rtsp://{HOST}:{RTSP_PORT}")
    check("200" in st, f"OPTIONS {st}")
    check("announce" in hd.get("public", "").lower(), f"Public={hd.get('public')}")

    st, hd, sd = rtsp("DESCRIBE", f"rtsp://{HOST}:{RTSP_PORT}")
    check("200" in st, f"DESCRIBE {st}")
    check(len(sd) > 0, f"DESCRIBE payload {len(sd)}B")
    check(b"H264/90000" in sd, "sdp H264")
    for bad in (b"AV1/90000", b"sprop-parameter-sets=AAAAAU",
                b"x-nv-video[0].refPicInvalidation"):
        check(bad not in sd, f"sdp без {bad.decode()}")
    # encryptionSupported:1 = только SS_ENC_CONTROL_V2 (control шифруется, медиа plaintext);
    # encryptionRequested не шлём — иначе клиент может вынудить видеошифрование
    check(b"x-ss-general.encryptionSupported:1" in sd, "sdp encryptionSupported:1 (CONTROL_V2)")
    check(b"encryptionRequested" not in sd, "sdp без encryptionRequested")

    st, hd_a, _ = rtsp("SETUP", "rtsp://%s:%d/streamid=audio/0/0" % (HOST, RTSP_PORT))
    check("200" in st, f"SETUP audio {st}")
    sess = hd_a.get("session", "")
    check(sess.split(";")[0].strip() == "DEADBEEFCAFE", f"Session={sess!r}")
    check(f"server_port=49002" in hd_a.get("transport", ""), f"Transport={hd_a.get('transport')}")
    ping_hex = hd_a.get("x-ss-ping-payload", "")
    check(len(ping_hex) == 16 and all(c in "0123456789abcdefABCDEF" for c in ping_hex),
          f"X-SS-Ping-Payload={ping_hex!r} (16 hex)")

    st, hd_v, _ = rtsp("SETUP", "rtsp://%s:%d/streamid=video/0/0" % (HOST, RTSP_PORT))
    check("200" in st, f"SETUP video {st}")
    check("server_port=49000" in hd_v.get("transport", ""), f"video port {hd_v.get('transport')}")
    check(hd_v.get("x-ss-ping-payload") == ping_hex, "video ping payload тот же")

    st, hd_c, _ = rtsp("SETUP", "rtsp://%s:%d/streamid=control/13/0" % (HOST, RTSP_PORT))
    check("200" in st, f"SETUP control {st}")
    ccd = hd_c.get("x-ss-connect-data", "")
    check(ccd.isdigit(), f"X-SS-Connect-Data={ccd!r} (dec u32)")

    sdp = (
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=NxSim\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n"
        "a=x-nv-video[0].packetSize:1024 \r\n"
        "a=x-nv-video[0].maxFPS:60 \r\n"
        "a=x-nv-video[0].clientViewportWd:1280 \r\n"
        "a=x-nv-video[0].clientViewportHt:720 \r\n"
        "a=x-nv-vqos[0].fec.minRequiredFecPackets:0 \r\n"
        "a=x-nv-general.useReliableUdp:0 \r\n"
        "a=x-ss-general.encryptionEnabled:0 \r\n"
    ).encode()
    st, _, _ = rtsp("ANNOUNCE", f"rtsp://{HOST}:{RTSP_PORT}", body=sdp,
                    extra_headers=[f"Session: {sess}"])
    check("200" in st, f"ANNOUNCE {st}")

    st, _, _ = rtsp("PLAY", f"rtsp://{HOST}:{RTSP_PORT}", extra_headers=[f"Session: {sess}"])
    check("200" in st, f"PLAY {st}")

    print("== 3) SS_PING → RTP ==")
    # Клиент шлёт X-SS-Ping-Payload как 16 ASCII-символов (не декодированный hex!)
    ping = ping_hex.encode("ascii")
    me = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    me.bind(("0.0.0.0", 50000))
    me.settimeout(0.5)
    for seq in range(6):
        me.sendto(ping + struct.pack(">I", seq), (HOST, VIDEO_PORT))
        time.sleep(0.2)

    pkts = []
    deadline = time.time() + 8
    while time.time() < deadline and len(pkts) < 200:
        try:
            data, src = me.recvfrom(2048)
        except socket.timeout:
            continue
        if len(data) == 20 and data[:16] == ping:
            me.sendto(ping + struct.pack(">I", 99), (HOST, VIDEO_PORT))
            continue
        pkts.append((data, src))

    check(len(pkts) > 10, f"получено RTP-пакетов: {len(pkts)}")
    if not pkts:
        print("RTP не пришёл — смотреть /tmp/nx-server.log")
        sys.exit(1)

    print(f"  источник RTP: {pkts[0][1]}")
    frames = {}
    prev_ts = -1
    for i, (d, _) in enumerate(pkts):
        if len(d) != BLOCKSIZE:
            check(False, f"пакет {i}: длина {len(d)} != {BLOCKSIZE}")
            continue
        seq = struct.unpack(">H", d[2:4])[0]
        ts = struct.unpack(">I", d[4:8])[0]
        ssrc = struct.unpack(">I", d[8:12])[0]
        spi = struct.unpack("<I", d[16:20])[0]
        fi = struct.unpack("<I", d[20:24])[0]
        flags, extra, mff, mfb = d[24], d[25], d[26], d[27]
        fec = struct.unpack("<I", d[28:32])[0]
        idx = spi >> 8  # lowseq + x
        x = (fec >> 12) & 0x3FF
        ds = (fec >> 22) & 0x3FF
        frames.setdefault(fi, []).append((x, flags, ds, d[32:]))
        if ts != prev_ts and prev_ts >= 0:
            check(ts > prev_ts, f"пакет {i}: ts {ts} > {prev_ts} (нашёл новый кадр)")
        prev_ts = ts
        if i >= 20:
            continue  # глубже первых 20 каждое поле не валидируем (шум в выводе)
        check(d[0] == 0x90, f"пакет {i}: header=0x{d[0]:02x}")
        check(d[1] == 0, f"пакет {i}: packetType={d[1]}")
        check(ssrc == 0, f"пакет {i}: ssrc={ssrc}")
        check((spi & 0xFF) == 0, f"пакет {i}: spi младшие 8 бит = 0")
        check(seq == (idx & 0xFFFF), f"пакет {i}: seq {seq} == (spi>>8)&0xffff {idx & 0xFFFF}")
        check(flags & 0x1, f"пакет {i}: PIC_DATA (flags=0x{flags:02x})")
        check(extra == 0 and mff == 0x10, f"пакет {i}: extra=0x{extra:02x} multiFecFlags=0x{mff:02x}")
        blocks = ((mfb >> 4) & 0x3) + 1
        block_idx = mfb >> 6
        check(blocks == 1 and block_idx == 0,
              f"пакет {i}: multiFecBlocks=0x{mfb:02x} (blocks={blocks}, idx={block_idx})")
        check((fec & 0xF) == 0, f"пакет {i}: fecInfo percentage=0")
        print(f"    pkt{i}: seq={seq} ts={ts} spi=0x{spi:08x} fi={fi} flags=0x{flags:02x} "
              f"mff=0x{mff:02x} mfb=0x{mfb:02x} fec=0x{fec:08x} x={x} ds={ds} "
              f"payload={len(d) - 32}")

    # непрерывность seq по порядку прихода
    seqs = [struct.unpack(">H", d[2:4])[0] for d, _ in pkts[:60]]
    gaps = [(a, b) for a, b in zip(seqs, seqs[1:]) if (b - a) % 65536 != 1]
    check(not gaps, f"seq непрерывен (первые 60, gaps={gaps[:3]})")

    # собираем кадр с флагом SOF/EOF → Annex-B
    good = [fi for fi, fl in frames.items()
            if any(f & 0x2 for _, f, _, _ in fl) and any(f & 0x4 for _, f, _, _ in fl)]
    check(len(good) > 0, f"кадры с SOF+EOF: {sorted(good)[:5]}")
    if good:
        fi = sorted(good)[0]
        chunks = sorted(frames[fi])  # сортировка по x (первый элемент кортажа)
        raw = b"".join(c for _, _, _, c in chunks if c)
        # первый чанк несёт 8B video_short_frame_header перед AU (как у клиента)
        check(raw[0] == 0x01, f"кадр {fi}: frame_header headerType=0x{raw[0]:02x}")
        fh_ft = raw[3]
        fh_last = struct.unpack("<H", raw[4:6])[0]
        check(fh_ft in (1, 2, 5), f"кадр {fi}: frameType={fh_ft}")
        check(fh_last > 0 and fh_last <= 1008, f"кадр {fi}: lastPayloadLen={fh_last}")
        # L = (chunks-1)*1008 + last - 8; хвост zero-pad отрезаем по lastPayloadLen
        total = (len(chunks) - 1) * (PACKET_SIZE - 16) + fh_last - 8
        data = raw[8:8 + total]
        check(len(data) == total, f"кадр {fi}: AU {len(data)}B (обрезка по lastPayloadLen)")
        check(data.startswith(b"\x00\x00\x00\x01") or data.startswith(b"\x00\x00\x01"),
              f"кадр {fi}: Annex-B start code ({data[:8].hex()})")
        nals = []
        b = data
        pos = []
        i = 0
        while i + 3 <= len(b):
            if b[i:i + 3] == b"\x00\x00\x01":
                pos.append(i)
                i += 3
            else:
                i += 1
        types = [b[p + 3] & 0x1F for p in pos if p + 3 < len(b)]
        print(f"    кадр {fi}: {len(chunks)} чанков, {len(pos)} NAL, types={types[:8]}")
        check(7 in types, "в кадре есть SPS (type 7)")

    me.close()
    print("== 4) TEARDOWN ==")
    st, _, _ = rtsp("TEARDOWN", f"rtsp://{HOST}:{RTSP_PORT}", extra_headers=[f"Session: {sess}"])
    check("200" in st, f"TEARDOWN {st}")

    print()
    if failures:
        print(f"FAIL ({len(failures)}):")
        for f in failures:
            print("  -", f)
        sys.exit(1)
    print("stream_sim: ALL PASS")


if __name__ == "__main__":
    main()
