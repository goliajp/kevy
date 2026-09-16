"""What a terminal shows after a byte stream — cligate's screen mode.

The REPL's contract is what a person sees, not the escape sequences that
redraw it: redis-cli rewrites the whole line on every key, kevy-cli may do
less. So a screen case feeds each CLI's output through this model and
compares the screens.

The model is a VT100 subset: printable bytes at the cursor with wrap at the
right margin, CR, LF (as a terminal with ONLCR shows it: a new line), BS, and
CSI A/B/C/D (moves), H (home, or row;col), J (erase below / whole screen), K
(erase right / left / line), m (SGR, kept per cell as the attribute string so
a grey hint and a plain word are different screens). Anything else fails the
case rather than being ignored: an unmodelled sequence is a question the
model cannot answer.
"""

import re

COLS = 80
CSI = re.compile(rb"\x1b\[([0-9;?]*)([A-Za-z])")


class Screen:
    def __init__(self, cols=COLS):
        self.cols = cols
        self.rows = [[]]
        self.row = self.col = 0
        self.attr = ""

    def _line(self):
        while len(self.rows) <= self.row:
            self.rows.append([])
        return self.rows[self.row]

    def put(self, ch):
        if self.col >= self.cols:
            self.row, self.col = self.row + 1, 0
        line = self._line()
        while len(line) <= self.col:
            line.append((" ", ""))
        line[self.col] = (ch, self.attr)
        self.col += 1

    def csi(self, params, final):
        nums = [int(p) if p.isdigit() else 0 for p in params.split(";")] if params else []
        n = nums[0] if nums and nums[0] else 1
        if final == "A":
            self.row = max(0, self.row - n)
        elif final == "B":
            self.row += n
        elif final == "C":
            self.col = min(self.cols - 1, self.col + n)
        elif final == "D":
            self.col = max(0, self.col - n)
        elif final == "H":
            self.row = (nums[0] - 1) if len(nums) > 0 and nums[0] else 0
            self.col = (nums[1] - 1) if len(nums) > 1 and nums[1] else 0
        elif final == "J":
            mode = nums[0] if nums else 0
            if mode == 2:
                self.rows = [[] for _ in self.rows]
            else:
                del self._line()[self.col:]
                del self.rows[self.row + 1:]
        elif final == "K":
            mode = nums[0] if nums else 0
            line = self._line()
            if mode == 0:
                del line[self.col:]
            elif mode == 1:
                line[: self.col] = [(" ", "")] * min(self.col, len(line))
            else:
                line.clear()
        elif final == "m":
            self.attr = "" if params in ("", "0") else params
        else:
            raise ValueError(f"unmodelled escape sequence ESC[{params}{final}")

    def feed(self, data: bytes):
        i = 0
        while i < len(data):
            b = data[i]
            if b == 0x1B:
                m = CSI.match(data, i)
                if not m:
                    raise ValueError(f"unmodelled escape at byte {i}: {data[i:i + 8]!r}")
                self.csi(m.group(1).decode(), m.group(2).decode())
                i = m.end()
                continue
            if b == 0x0D:
                self.col = 0
            elif b == 0x0A:
                self.row, self.col = self.row + 1, 0
            elif b == 0x08:
                self.col = max(0, self.col - 1)
            elif b == 0x07:
                pass  # the bell is heard, not seen
            else:
                self.put(chr(b))
            i += 1
        return self

    def render(self) -> str:
        """Rows with trailing blanks dropped; an attribute run is written
        `{attr|text}` so colour differences show in a diff."""
        out = []
        for line in self.rows:
            text, cur = "", ""
            for ch, attr in line:
                if attr != cur:
                    text += "}" if cur else ""
                    text += f"{{{attr}|" if attr else ""
                    cur = attr
                text += ch
            text += "}" if cur else ""
            out.append(text.rstrip())
        while out and not out[-1]:
            out.pop()
        return "\n".join(out) + "\n"


def screen(data: bytes) -> bytes:
    return Screen().feed(data).render().encode()
