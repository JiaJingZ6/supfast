"""生成 supfast 的 icon.ico —— 4 根竖条，最后一根用强调橙（对应界面左上角那个标记）。
不依赖任何第三方库：手写 BMP DIB + ICO 头。"""
import struct, os

S = 256
BG = (0x11, 0x16, 0x1a)
BARS = [(0x8b, 0x94, 0x99), (0x8b, 0x94, 0x99), (0x8b, 0x94, 0x99), (0xc2, 0x41, 0x0c)]

# 画布：BGRA，原点左上
px = [[(BG[2], BG[1], BG[0], 255) for _ in range(S)] for _ in range(S)]

bw, bh, gap = 22, 104, 16
total = len(BARS) * bw + (len(BARS) - 1) * gap
x0 = (S - total) // 2
y0 = (S - bh) // 2
for i, c in enumerate(BARS):
    xs = x0 + i * (bw + gap)
    for y in range(y0, y0 + bh):
        for x in range(xs, xs + bw):
            px[y][x] = (c[2], c[1], c[0], 255)

# XOR 位图：自下而上
xor = bytearray()
for y in range(S - 1, -1, -1):
    for x in range(S):
        b, g, r, a = px[y][x]
        xor += bytes((b, g, r, a))
# AND 掩码：全 0（不透明），每行按 4 字节对齐
row = ((S + 31) // 32) * 4
and_mask = bytes(row * S)

dib = struct.pack('<IiiHHIIiiII', 40, S, S * 2, 1, 32, 0, len(xor), 0, 0, 0, 0)
img = dib + bytes(xor) + and_mask

icondir = struct.pack('<HHH', 0, 1, 1)
entry = struct.pack('<BBBBHHII', 0, 0, 0, 0, 1, 32, len(img), 6 + 16)
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'icons')
os.makedirs(out, exist_ok=True)
with open(os.path.join(out, 'icon.ico'), 'wb') as f:
    f.write(icondir + entry + img)
print('icon.ico', len(icondir + entry + img), 'bytes')
