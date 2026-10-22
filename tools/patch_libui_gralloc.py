#!/usr/bin/env python3
"""Патч libui.so (GSI/Treble на MTK): снять запрет бита usage 0x2000.

Контекст: MTK-декодер просит у SurfaceView буферы с usage, где стоит
GRALLOC_USAGE_HW_CAMERA_WRITE (0x2000). Фреймворк AOSP (Gralloc4::Mapper)
режет этот бит маской 0xFFFF00282400 и отдаёт -EINVAL:

    E Gralloc4: buffer descriptor contains invalid usage bits 0x2000
    E GraphicBufferAllocator: Failed to allocate (1088 x 800) ... usage 402933: -22
    E BufferQueueProducer: [SurfaceView - com.limelight/...Game#0] dequeueBuffer:
        createGraphicBuffer failed

Из-за этого HW-декодер не может отдать ни одного кадра: клиент роняет кадры
("Video decode unit queue overflow") и просит IDR ("Waiting for IDR frame").

Патч: в двух местах (validateBufferDescriptorInfo и createDescriptor) маска
0x2400 -> 0x0400, т.е. бит 0x2000 больше не запрещён. Вендорный gralloc MTK этот
бит знает и умеет аллоцировать с ним.

Запуск: python3 patch_libui_gralloc.py <оригинальный libui.so> <выход>
"""
import sys

# Gralloc4::Mapper::validateBufferDescriptorInfo / createDescriptor:
#   mov x9, #0x2400  ->  mov x9, #0x0400
PATCHES = [
    (0x2E72C, bytes.fromhex("098084d2"), bytes.fromhex("098080d2")),
    (0x2E788, bytes.fromhex("098084d2"), bytes.fromhex("098080d2")),
]


def main(src: str, dst: str) -> int:
    data = bytearray(open(src, "rb").read())
    for off, old, new in PATCHES:
        if data[off:off + 4] != old:
            print(f"SKIP {hex(off)}: там {data[off:off+4].hex()}, ожидали {old.hex()}")
            continue
        data[off:off + 4] = new
        print(f"OK {hex(off)}: {old.hex()} -> {new.hex()}")
    open(dst, "wb").write(bytes(data))
    print("записано:", dst)
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(sys.argv[1], sys.argv[2]))
