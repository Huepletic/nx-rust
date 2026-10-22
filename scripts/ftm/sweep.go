// Перебор ioctl-команд /dev/charger_ftm с детектором эффекта.
//
// Команды FTM неизвестны, ENOTTY не работает (драйвер возвращает 0 на всё),
// поэтому критерий отбора — изменение состояния зарядки, читаемое из sysfs:
//   /sys/class/power_supply/battery/status   ("Not charging" -> "Charging")
//   /sys/class/power_supply/usb/current_max
//   /sys/class/platform/charger/{chg1_current,chg2_current,input_current}
//
// Запуск: ftm sweep [arg]   — arg по умолчанию 0 (безобидный)
// Печатает только те коды, после которых состояние изменилось.

package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"syscall"
	"unsafe"
)

const dev = "/dev/charger_ftm"

func read(path string) string {
	b, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

type snap struct {
	status, vmax, chg1, chg2, inCur string
}

func take() snap {
	return snap{
		status: read("/sys/class/power_supply/battery/status"),
		vmax:   read("/sys/class/power_supply/usb/current_max"),
		chg1:   read("/sys/devices/platform/charger/chg1_current"),
		chg2:   read("/sys/devices/platform/charger/chg2_current"),
		inCur:  read("/sys/devices/platform/charger/input_current"),
	}
}

func (s snap) str() string {
	return fmt.Sprintf("status=%s vmax=%s chg1=%s chg2=%s in=%s",
		s.status, s.vmax, s.chg1, s.chg2, s.inCur)
}

func call(code uintptr, arg int32) {
	f, err := os.OpenFile(dev, os.O_RDWR, 0)
	if err != nil {
		return
	}
	defer f.Close()
	v := arg
	syscall.Syscall(syscall.SYS_IOCTL, f.Fd(), code,
		uintptr(unsafe.Pointer(&v)))
}

func main() {
	arg := int32(0)
	if len(os.Args) > 2 {
		v, _ := strconv.ParseInt(os.Args[2], 0, 64)
		arg = int32(v)
	}
	// _IO('c',n)=0x4300+n, _IOR=0x80004300+n, _IOW=0x40004300+n, _IOWR=0xc0004300+n
	// Перебираем все буквы-команды, т.к. номер ioctl у MTK неизвестен.
	fams := []struct {
		base uintptr
		name string
	}{
		{0x4300, "_IO"},
		{0x80004300, "_IOR"},
		{0x40004300, "_IOW"},
		{0xc0004300, "_IOWR"},
	}
	base := take()
	fmt.Printf("стартовое состояние: %s\n", base.str())
	hits := 0
	for c := 'a'; c <= 'z'; c++ {
		for _, fam := range fams {
			for n := 0; n < 32; n++ {
				code := fam.base | (uintptr(c) << 8) | uintptr(n)
				call(code, arg)
				now := take()
				if now != base {
					hits++
					fmt.Printf("!!! ИЗМЕНЕНИЕ %s('%c',%d) code=0x%08x: %s -> %s\n",
						fam.name, c, n, code, base.str(), now.str())
					base = now
				}
			}
		}
	}
	fmt.Printf("проверено кодов: %d, с эффектом: %d\n", 26*4*32, hits)
}
