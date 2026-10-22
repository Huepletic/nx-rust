// Freestanding aarch64 helper: вызывает ioctl на /dev/charger_ftm.
// Без libc (только raw syscalls) — собирается кросс-кроссом clang'ом
// без sysroot'а и запускается на планшете через adb.
//
// Использование: ftm <ioctl_code_hex> [arg_int]
// Печатает: open_rc=<n> ioctl_rc=<n> arg_after=<n>

typedef unsigned long ulong;
typedef long slong;

static inline slong sys3(slong n, slong a, slong b, slong c) {
	register slong x8 __asm__("x8") = n;
	register slong x0 __asm__("x0") = a;
	register slong x1 __asm__("x1") = b;
	register slong x2 __asm__("x2") = c;
	__asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
	return x0;
}

static inline slong sys4(slong n, slong a, slong b, slong c, slong d) {
	register slong x8 __asm__("x8") = n;
	register slong x0 __asm__("x0") = a;
	register slong x1 __asm__("x1") = b;
	register slong x2 __asm__("x2") = c;
	register slong x3 __asm__("x3") = d;
	__asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
	return x0;
}

#define SYS_openat 56
#define SYS_ioctl 29
#define SYS_write 64
#define SYS_close 57
#define SYS_exit 93
#define AT_FDCWD (-100)
#define O_RDWR 2

static inline ulong get_sp(void) {
	ulong x;
	__asm__ volatile("mov %0, sp" : "=r"(x));
	return x;
}

static void out(const char *s) {
	slong len = 0;
	while (s[len]) len++;
	sys3(SYS_write, 1, (slong)s, len);
}

static void out_hex(ulong v) {
	char buf[19];
	buf[0] = '0';
	buf[1] = 'x';
	for (int i = 0; i < 16; i++) {
		int nib = (int)((v >> ((15 - i) * 4)) & 0xf);
		buf[2 + i] = (char)(nib < 10 ? '0' + nib : 'a' + nib - 10);
	}
	buf[18] = '\n';
	sys3(SYS_write, 1, (slong)buf, 19);
}

static ulong parse_hex(const char *s) {
	ulong v = 0;
	while (*s) {
		char c = *s++;
		int d;
		if (c >= '0' && c <= '9') d = c - '0';
		else if (c >= 'a' && c <= 'f') d = c - 'a' + 10;
		else if (c >= 'A' && c <= 'F') d = c - 'A' + 10;
		else break;
		v = (v << 4) | (ulong)d;
	}
	return v;
}

void _start(void) {
	ulong sp = get_sp();
	ulong argc = *(ulong *)sp;
	ulong *argv = (ulong *)sp + 1;
	const char *path = "/dev/charger_ftm";
	ulong code = 0;
	int val = 0;

	if (argc > 1) code = parse_hex((const char *)argv[1]);
	if (argc > 2) val = (int)parse_hex((const char *)argv[2]);

	slong fd = sys4(SYS_openat, AT_FDCWD, (slong)path, O_RDWR, 0);
	out("open_rc=");
	out_hex((ulong)fd);
	if (fd < 0) {
		sys3(SYS_exit, 1, 0, 0);
	}
	slong rc = sys3(SYS_ioctl, fd, (slong)code, (slong)&val);
	out("ioctl_rc=");
	out_hex((ulong)rc);
	out("arg_after=");
	out_hex((ulong)(ulong)val);
	sys3(SYS_close, fd, 0, 0);
	sys3(SYS_exit, 0, 0, 0);
}
