# Референсные исходники Sunshine

Файлы в этом каталоге — **копии исходников [Sunshine](https://github.com/LizardByte/Sunshine)**
(LizardByte), лицензия **GPL-3.0**. Они лежат здесь только как справочник по
протоколу GameStream/NX: pairing, NVHTTP, RTSP, control-канал, RTP, FEC.

| Файл | Что смотрим |
|---|---|
| `nvhttp.cpp`, `nvhttp.h` | HTTP/HTTPS endpoints, pairing, XML |
| `rtsp.cpp` | RTSP-хендшейк, SDP, порты |
| `stream.cpp`, `stream.h` | control (ENet), видео RTP, шифрование |
| `input.cpp` | пакеты ввода |
| `main.cpp` | константы портов |
| `crypto.cpp`, `crypto.h` | шифрование |
| `video.cpp` | видео-пайплайн |

Это **не часть сборки** `nx-server`: Rust-реализация независимая, файлы не
компилируются и не линкуются. Не переносите код из этого каталога в `src/`
без проверки лицензионной совместимости: весь проект и так под GPL-3.0, но
авторство конкретных фрагментов нужно сохранять.

При распространении репозитория сохраняйте этот каталог, `LICENSE` и
`NOTICE`; удалять копирайты Sunshine нельзя.

Оригинал: <https://github.com/LizardByte/Sunshine>.
