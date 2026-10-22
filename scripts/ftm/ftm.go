// Клиент ioctl для /dev/charger_ftm планшета (MTK mt6370 + SGM41513).
//
// Собрано статически под aarch64 без libc:
//   GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o ftm ftm.go
// и залито на планшет через adb.
//
// Режимы:
//   ftm sweep              — перебрать ioctl-коды семейства _IO/_IOR/_IOW/_IOWR
//                            по 'c', печатая только те, что не вернули ENOTTY
//   ftm <code> [arg]       — один вызов: code — шестнадцатеричное значение ioctl,
//                            arg — целое значение аргумента
package main

import (
	"fmt"
	"os"
	"strconv"
	"syscall"
	"unsafe"
)

const dev = "/dev/charger_ftm"

func call(code uintptr, arg int32) (errno syscall.Errno, out int32) {
	f, err := os.OpenFile(dev, os.O_RDWR, 0)
	if err != nil {
		return syscall.EIO, arg
	}
	defer f.Close()
	v := arg
	_, _, e := syscall.Syscall(syscall.SYS_IOCTL, f.Fd(), code,
		uintptr(unsafe.Pointer(&v)))
	return e, v
}

func main() {
	if len(os.Args) < 2 {
		fmt.Println("usage: ftm sweep | ftm <code_hex> [arg]")
		os.Exit(2)
	}
	if os.Args[1] == "sweep" {
		// _IO('c',n)=0x4300+n  _IOR=0x80004300+n  _IOW=0x40004300+n
		// _IOWR=0xc0004300+n ; sizeof(int)=4
		fams := []uintptr{0x4300, 0x80004300, 0x40004300, 0xc0004300}
		names := []string{"_IO", "_IOR", "_IOW", "_IOWR"}
		for fi, base := range fams {
			for n := 0; n < 16; n++ {
				code := base + uintptr(n)
				e, v := call(code, 0)
				if e == syscall.ENOTTY {
					continue
				}
				fmt.Printf("%s('c',%2d) code=0x%08x errno=%d arg=%d\n",
					names[fi], n, code, int(e), v)
			}
		}
		return
	}
	code, err := strconv.ParseUint(os.Args[1], 0, 64)
	if err != nil {
		fmt.Println("bad code:", err)
		os.Exit(2)
	}
	var arg int32
	if len(os.Args) > 2 {
		v, _ := strconv.ParseInt(os.Args[2], 0, 64)
		arg = int32(v)
	}
	e, v := call(uintptr(code), arg)
	fmt.Printf("code=0x%08x errno=%d arg_after=%d\n", uint32(code), int(e), v)
}
