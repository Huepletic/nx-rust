#!/bin/bash
# Полноэкранные часы с мс на стрим-воркспейсе (HEADLESS-*).
# Требуется foot; путь к скрипту не захардкожен.
HERE="$(cd "$(dirname "$0")" && pwd)"
exec foot -a msclock -F -T msclock -f "monospace:size=90" -w 1088x800 \
  python3 -u "$HERE/ms_clock.py"
