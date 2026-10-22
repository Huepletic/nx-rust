#!/system/bin/sh
# Возврат RNDIS+ADB на планшете (после ребута планшет собирает только adb-only).
# Схема, которая работала 2026-10-20: classic rndis.gs4 (интерфейсы 0xE0 + 0x02)
# + ffs.adb (ff/42/01) → устройство перечисляется как 0x2005, adb цепляется
# без шима (классы интерфейсов различимы).
G=/config/usb_gadget/g1
exec >/data/local/tmp/fix_gadget3.log 2>&1
echo "=== props before ==="
getprop sys.usb.config; getprop persist.sys.usb.config; getprop vendor.usb.config; getprop sys.usb.state
echo "=== 1. отцепить UDC ==="
echo "" > $G/UDC
sleep 2
echo "=== 2. убрать старые линки ==="
rm -f $G/configs/b.1/f1 $G/configs/b.1/f2 $G/configs/b.1/f3
echo "=== 3. функции ==="
mkdir -p $G/functions/rndis.gs4 2>/dev/null
mkdir -p $G/functions/ffs.adb 2>/dev/null
ls $G/functions/ | grep -E "rndis|ffs" || echo "НЕТ ФУНКЦИЙ rndis/ffs"
echo "=== 4. линки ==="
ln -s $G/functions/rndis.gs4 $G/configs/b.1/f1 2>&1 || echo "ln f1 fail"
ln -s $G/functions/ffs.adb $G/configs/b.1/f2 2>&1 || echo "ln f2 fail"
ls -l $G/configs/b.1/ | grep -E "f[0-9]"
echo "=== 5. id ==="
echo 0x0E8D > $G/idVendor
echo 0x2005 > $G/idProduct
echo "rndis_adb" > $G/configs/b.1/strings/0x409/configuration
echo "=== 6. подцепить UDC ==="
echo musb-hdrc > $G/UDC
sleep 3
cat $G/UDC
getprop sys.usb.state
echo "=== 7. итог ==="
ls -l $G/configs/b.1/ | grep -E "f[0-9]"
