import sys
import time

# Живые часы хоста с точностью до миллисекунд. Рисуем в верхней строке
# терминала, чтобы кадр стрима содержал однозначное время отрисовки.
out = sys.stdout
out.write("\033[2J\033[H\033[?25l")
out.flush()
last = None
while True:
    t = time.time()
    ms = int((t % 1.0) * 1000.0)
    line = time.strftime("%H:%M:%S", time.localtime(t)) + f".{ms:03d}"
    if line != last:
        out.write("\033[H" + line)
        out.flush()
        last = line
    time.sleep(0.002)
